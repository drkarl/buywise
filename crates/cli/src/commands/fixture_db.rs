//! In-memory fixture database for fixture-mode `optimise` and `validate-basket`.
//!
//! Fixture mode (per issue #11) builds a private in-memory SQLite database,
//! applies **only** `migrations/001_phase0_schema.sql` (not the 002 seed), and
//! seeds it from the committed fixtures:
//!
//!   * `fixtures/canonical_items.json` — categories, canonical_items, item_aliases
//!   * `fixtures/item_constraints.json` — item_constraints rows
//!   * `fixtures/offers.json` — retailer_products + product_offers (via the
//!     BasketWatch ingest adapter's pure [`normalise`], so unit prices are
//!     validated exactly as in live ingest)
//!   * `fixtures/product_matches.csv` — product_matches rows
//!
//! No persistent DB file is created. Determinism: a fixed `fetched_at` and a
//! fixed run number (1) are used so freshness is `High` and results are stable.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_ingest::basketwatch::{self, RawBasketWatchProduct};
use serde::Deserialize;
use sqlx::sqlite::SqlitePool;

use crate::commands::seed_matches::{parse_matches_csv, CsvMatch};

/// The fixed snapshot timestamp used when seeding fixture offers.
const FIXTURE_FETCHED_AT: &str = "2026-05-29T10:00:00.000Z";
/// The fixed run number stamped on fixture offers and reported as the current
/// run, so every fixture offer reads as `FreshnessConfidence::High`.
const FIXTURE_RUN_NUMBER: i64 = 1;

/// Minimal mirror of `fixtures/canonical_items.json` for seeding.
#[derive(Debug, Deserialize)]
struct CanonicalItemsFixture {
    /// The household id embedded in the fixture. Intentionally **not** used to
    /// stamp rows: fixture mode seeds the catalogue under the *requested*
    /// household id instead (see [`seed_categories_and_items`]). Kept so the
    /// field is accepted (and documented) when the JSON is deserialised.
    #[allow(dead_code)]
    household_id: String,
    created_at: String,
    canonical_items: Vec<FixtureItem>,
}

#[derive(Debug, Deserialize)]
struct FixtureItem {
    id: String,
    name_en: String,
    name_es: String,
    category_id: String,
    default_constraint_policy: String,
    aliases: Vec<FixtureAlias>,
}

#[derive(Debug, Deserialize)]
struct FixtureAlias {
    id: String,
    alias: String,
    language: String,
    source: String,
}

/// Minimal mirror of `fixtures/item_constraints.json` for seeding.
#[derive(Debug, Deserialize)]
struct ItemConstraintsFixture {
    item_constraints: Vec<serde_json::Value>,
}

/// Build a fully-seeded in-memory fixture database from the committed fixtures
/// rooted at `root` (the workspace root). Returns the live pool.
pub async fn build_fixture_pool(root: &Path, household_id: &str) -> Result<SqlitePool> {
    let pool = db::connect("sqlite::memory:")
        .await
        .context("connecting in-memory fixture database")?;

    apply_schema_001(&pool, root).await?;
    // Fixture mode deliberately applies only 001 + 007, skipping migrations
    // 002–006. This works because every table/column the fixtures touch
    // (canonical_items, item_constraints, retailer_products, product_offers,
    // product_matches, source_runs, …) is created in 001_phase0_schema.sql;
    // 002–006 are data seeds / unrelated feature tables the fixture path never
    // reads. Backlog-T1 then adds retailer_products.form_factor / dietary_flags
    // in 007, which we must apply too — otherwise upsert_retailer_product (which
    // now writes the two columns) would fail against the older table shape. If a
    // future migration moves any fixture-relevant DDL out of 001, this list must
    // be revisited.
    apply_schema_file(&pool, root, "007_retailer_product_attributes.sql").await?;
    // Issue #78: the optimiser now reads `match_feedback` (created in migration
    // 003) to honour a household's confirmed `prefer` choice for ask-policy items.
    // Fixture mode never seeds feedback, but the SELECT must find the table, so we
    // create the minimal `match_feedback` shape here rather than applying all of
    // 003 (which would pull in unrelated Phase-1 auth tables). The fixture catalogue
    // has no ask-policy items today, so this table simply stays empty.
    create_match_feedback_table(&pool).await?;
    seed_categories_and_items(&pool, root, household_id).await?;
    seed_item_constraints(&pool, root).await?;
    seed_offers(&pool, root).await?;
    seed_matches(&pool, root, household_id).await?;

    Ok(pool)
}

/// Apply `migrations/001_phase0_schema.sql` only (no 002 seed).
///
/// The whole file is executed as one script via [`sqlx::raw_sql`], which runs
/// every statement on a single connection. Naively splitting on `;` is wrong
/// here — the schema's header comment contains semicolons — and would corrupt
/// the DDL.
async fn apply_schema_001(pool: &SqlitePool, root: &Path) -> Result<()> {
    apply_schema_file(pool, root, "001_phase0_schema.sql").await
}

/// Apply a single migration file by name as one raw-SQL script.
///
/// Shared by [`apply_schema_001`] and the additive Backlog-T1 migration so
/// fixture mode tracks the columns the optimiser and writers expect. The whole
/// file runs on a single connection via [`sqlx::raw_sql`]; statements are never
/// split on `;` (a header comment may contain one).
async fn apply_schema_file(pool: &SqlitePool, root: &Path, file: &str) -> Result<()> {
    let path = root.join("migrations").join(file);
    let sql =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    sqlx::raw_sql(&sql)
        .execute(pool)
        .await
        .with_context(|| format!("applying {}", path.display()))?;
    Ok(())
}

/// Create the minimal `match_feedback` table the optimiser's `prefer`-resolution
/// SELECT depends on (issue #78). This mirrors the DDL from
/// `migrations/003_phase1_schema.sql` verbatim *except* it omits the
/// `REFERENCES users(id)` clause: fixture mode never creates the `users` table
/// (it lives in 003 alongside the rest of the Phase-1 auth schema the fixture
/// path doesn't need), and the optimiser only ever reads this table — it inserts
/// no rows — so the foreign key would only ever be checked against an empty table.
/// If migration 003's `match_feedback` shape changes, update this to match.
async fn create_match_feedback_table(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(
        "CREATE TABLE match_feedback (\
             id                TEXT PRIMARY KEY,\
             product_match_id  TEXT NOT NULL REFERENCES product_matches(id),\
             user_id           TEXT NOT NULL,\
             action            TEXT NOT NULL CHECK(action IN ('accept', 'reject', 'prefer')),\
             created_at        TEXT NOT NULL\
         );",
    )
    .execute(pool)
    .await
    .context("creating fixture match_feedback table")?;
    Ok(())
}

/// Seed `categories`, `canonical_items`, and `item_aliases` from
/// `fixtures/canonical_items.json` and the category list in `dictionary.json`.
///
/// Catalogue rows are stamped with the **requested** `household_id`, *not* the
/// id embedded in `canonical_items.json`. Resolution (`matching.rs`) and pricing
/// (`product_matches`, seeded by [`seed_matches`]) both filter by the requested
/// household, so stamping the catalogue with a different id would split the
/// fixture DB across two households and silently return empty results for any
/// non-default `--household-id`. The canonical item *ids* are kept verbatim so
/// `product_matches.csv` still references them by their literal id.
async fn seed_categories_and_items(
    pool: &SqlitePool,
    root: &Path,
    household_id: &str,
) -> Result<()> {
    // Categories live in dictionary.json (canonical_items.json carries only the
    // category_id reference). Seed them first to satisfy the FK.
    let dict_path = root.join("fixtures").join("dictionary.json");
    let dict = crate::generate::load_dictionary(&dict_path)?;
    for c in &dict.categories {
        sqlx::query(
            "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&c.uuid)
        .bind(&c.name_en)
        .bind(&c.name_es)
        .bind(&c.icon)
        .bind(c.default_order)
        .execute(pool)
        .await
        .with_context(|| format!("seeding category {}", c.key))?;
    }

    let items_path = root.join("fixtures").join("canonical_items.json");
    let raw = std::fs::read_to_string(&items_path)
        .with_context(|| format!("reading {}", items_path.display()))?;
    let fixture: CanonicalItemsFixture =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", items_path.display()))?;

    for item in &fixture.canonical_items {
        sqlx::query(
            "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&item.id)
        .bind(household_id)
        .bind(&item.name_en)
        .bind(&item.name_es)
        .bind(&item.category_id)
        .bind(&item.default_constraint_policy)
        .bind(&fixture.created_at)
        .execute(pool)
        .await
        .with_context(|| format!("seeding canonical item {}", item.id))?;

        for alias in &item.aliases {
            sqlx::query(
                "INSERT INTO item_aliases \
                 (id, household_id, canonical_item_id, alias, language, source, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(&alias.id)
            .bind(household_id)
            .bind(&item.id)
            .bind(&alias.alias)
            .bind(&alias.language)
            .bind(&alias.source)
            .bind(&fixture.created_at)
            .execute(pool)
            .await
            .with_context(|| format!("seeding alias {}", alias.id))?;
        }
    }
    Ok(())
}

/// Seed `item_constraints` from `fixtures/item_constraints.json`. JSON columns
/// are stored as their JSON TEXT representation (the schema columns are plain
/// TEXT); the optimiser does not enforce them in Phase 0.
async fn seed_item_constraints(pool: &SqlitePool, root: &Path) -> Result<()> {
    let path = root.join("fixtures").join("item_constraints.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let fixture: ItemConstraintsFixture =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    for entry in &fixture.item_constraints {
        let obj = entry
            .as_object()
            .context("each item_constraints entry must be a JSON object")?;
        let s =
            |k: &str| -> Option<String> { obj.get(k).and_then(|v| v.as_str()).map(str::to_string) };
        let f = |k: &str| -> Option<f64> { obj.get(k).and_then(serde_json::Value::as_f64) };
        let i = |k: &str| -> Option<i64> { obj.get(k).and_then(serde_json::Value::as_i64) };
        // JSON array columns are stored verbatim as JSON TEXT.
        let json_text = |k: &str| -> Option<String> {
            obj.get(k).filter(|v| !v.is_null()).map(|v| v.to_string())
        };

        sqlx::query(
            "INSERT INTO item_constraints \
             (id, canonical_item_id, brand, allowed_brands, excluded_brands, \
              preferred_size_value, preferred_size_basis, min_size_value, min_size_basis, \
              max_size_value, max_size_basis, pack_count, form_factor, dietary_flags, \
              substitution_policy, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .bind(s("id"))
        .bind(s("canonical_item_id"))
        .bind(s("brand"))
        .bind(json_text("allowed_brands"))
        .bind(json_text("excluded_brands"))
        .bind(f("preferred_size_value"))
        .bind(s("preferred_size_basis"))
        .bind(f("min_size_value"))
        .bind(s("min_size_basis"))
        .bind(f("max_size_value"))
        .bind(s("max_size_basis"))
        .bind(i("pack_count"))
        .bind(s("form_factor"))
        .bind(json_text("dietary_flags"))
        .bind(s("substitution_policy").unwrap_or_else(|| "any".to_string()))
        .bind(s("updated_at").unwrap_or_default())
        .execute(pool)
        .await
        .with_context(|| format!("seeding item_constraints {}", s("id").unwrap_or_default()))?;
    }
    Ok(())
}

/// Seed `retailer_products` + `product_offers` from `fixtures/offers.json`,
/// reusing the BasketWatch adapter's pure [`normalise`] so the fixture path
/// exercises the same unit-price validation as live ingest. A source_runs row
/// is inserted so the offer FK (`last_successful_source_run_id`) is satisfiable;
/// offers are stamped with the fixed [`FIXTURE_RUN_NUMBER`].
async fn seed_offers(pool: &SqlitePool, root: &Path) -> Result<()> {
    let path = root.join("fixtures").join("offers.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let products: Vec<RawBasketWatchProduct> =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    let normalised = basketwatch::normalise(products, FIXTURE_FETCHED_AT);

    // A source_runs row so product_offers.last_successful_source_run_id resolves.
    let run_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO source_runs \
         (id, source, started_at, finished_at, requests_made, items_fetched, items_normalised, \
          status, successful_run_number) \
         VALUES (?1, 'basketwatch', ?2, ?2, 0, ?3, ?3, 'success', ?4)",
    )
    .bind(&run_id)
    .bind(FIXTURE_FETCHED_AT)
    .bind(normalised.products.len() as i64)
    .bind(FIXTURE_RUN_NUMBER)
    .execute(pool)
    .await
    .context("seeding fixture source_runs row")?;

    let mut conn = pool.acquire().await.context("acquire fixture connection")?;
    for product in &normalised.products {
        db::upsert_retailer_product(&mut conn, product)
            .await
            .context("seeding fixture retailer product")?;
    }
    for draft in &normalised.offers {
        let offer = draft.clone().into_offer(&run_id, FIXTURE_RUN_NUMBER);
        db::insert_product_offer(&mut conn, &offer)
            .await
            .context("seeding fixture product offer")?;
    }
    Ok(())
}

/// Seed `product_matches` from `fixtures/product_matches.csv`, validating that
/// every referenced canonical item and retailer product exists (so a typo in
/// the CSV fails loudly rather than silently producing an unpriceable basket).
async fn seed_matches(pool: &SqlitePool, root: &Path, household_id: &str) -> Result<()> {
    let path = root.join("fixtures").join("product_matches.csv");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let matches = parse_matches_csv(&raw).with_context(|| format!("parsing {}", path.display()))?;

    // Build existence sets once for clear validation errors.
    let known_items: HashMap<String, ()> =
        sqlx::query_scalar::<_, String>("SELECT id FROM canonical_items")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|id| (id, ()))
            .collect();
    let known_products: HashMap<String, ()> =
        sqlx::query_scalar::<_, String>("SELECT id FROM retailer_products")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|id| (id, ()))
            .collect();

    for m in &matches {
        let CsvMatch {
            canonical_item_id,
            retailer_product_id,
            status,
            matched_by,
        } = m;
        if !known_items.contains_key(canonical_item_id) {
            bail!("product_matches.csv references unknown canonical_item_id {canonical_item_id:?}");
        }
        if !known_products.contains_key(retailer_product_id) {
            bail!(
                "product_matches.csv references unknown retailer_product_id {retailer_product_id:?} \
                 (is it in offers.json?)"
            );
        }
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO product_matches \
             (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, \
              created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        )
        .bind(&id)
        .bind(household_id)
        .bind(canonical_item_id)
        .bind(retailer_product_id)
        .bind(status)
        .bind(matched_by)
        .bind(FIXTURE_FETCHED_AT)
        .execute(pool)
        .await
        .context("seeding fixture product match")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `match_feedback` column as reported by `PRAGMA table_info`: the optimiser's
    /// `prefer`-resolution SELECT only reads columns, so the contract that must not
    /// drift is each column's name, declared type, NOT NULL flag, and primary-key
    /// flag. (FKs are intentionally not part of the comparison: the fixture omits the
    /// `REFERENCES users(id)` clause and `table_info` never reports foreign keys.)
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Column {
        name: String,
        type_decl: String,
        not_null: i64,
        pk: i64,
    }

    async fn match_feedback_columns(pool: &SqlitePool) -> Vec<Column> {
        let mut cols: Vec<Column> = sqlx::query_as::<
            _,
            (i64, String, String, i64, Option<String>, i64),
        >("PRAGMA table_info(match_feedback)")
        .fetch_all(pool)
        .await
        .expect("table_info(match_feedback)")
        .into_iter()
        .map(|(_cid, name, type_decl, not_null, _default, pk)| Column {
            name,
            type_decl,
            not_null,
            pk,
        })
        .collect();
        cols.sort();
        cols
    }

    /// Sync guard: the hand-written fixture `match_feedback` table must carry the
    /// same columns (name/type/NOT NULL/PK) as the canonical table from migration
    /// 003. If migration 003 changes the `match_feedback` shape, this fails so the
    /// fixture DDL (and the optimiser SELECT that depends on it) is updated in step.
    #[tokio::test]
    async fn fixture_match_feedback_columns_match_migration_003() {
        // Canonical table: a fully migrated DB (003 creates `match_feedback`).
        let migrated = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&migrated).await.expect("migrate");
        let expected = match_feedback_columns(&migrated).await;
        assert!(
            !expected.is_empty(),
            "migration 003 must create a match_feedback table"
        );

        // Fixture table: built standalone by the fixture path.
        let fixture = db::connect("sqlite::memory:").await.expect("connect");
        create_match_feedback_table(&fixture)
            .await
            .expect("create fixture match_feedback");
        let actual = match_feedback_columns(&fixture).await;

        assert_eq!(
            actual, expected,
            "fixture match_feedback columns have drifted from migration 003"
        );
    }
}
