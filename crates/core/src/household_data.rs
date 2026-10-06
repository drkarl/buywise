//! Household data controls: export and delete (issue #119, roadmap #114).
//!
//! Shared by the `buywise-cli` operator commands (`export-household` /
//! `delete-household`) and the member-facing API endpoints
//! (`crates/api/src/household.rs`, issue #136), so the two never drift:
//!
//! * [`export_household`] builds every household-scoped record as one JSON
//!   document. The layout is documented in `docs/household-export.md`; it is
//!   plain `serde_json::Value` (no app domain types) so the document is a
//!   stable, self-describing interchange format rather than a dump of internal
//!   structs.
//! * [`delete_household`] removes every household-scoped record in one
//!   transaction. `dry_run` runs the whole delete inside a transaction and
//!   ROLLS IT BACK, so it reports the exact rows a real run would remove without
//!   guessing at per-table predicates.
//!
//! Both functions are READ-ONLY against the schema (neither runs migrations) and
//! expect a database the API or an operator has already migrated. What is
//! household-scoped and what is not is listed in [`HOUSEHOLD_TABLES`] and
//! documented in `docs/data-flows.md` (Retention and deletion). Shared reference
//! data — `categories`, `subcategories`, `retailer_products`, `product_offers`,
//! `source_runs`, `api_usage_events` — is deliberately untouched by deletion: it
//! carries no household identity.

use std::collections::HashMap;

use serde_json::{json, Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Column, Row, SqlitePool};

use crate::db;

/// `format` field of the export document. Bump-incompatible changes accompany a
/// [`EXPORT_FORMAT_VERSION`] bump; consumers MUST check both.
pub const EXPORT_FORMAT: &str = "buywise-household-export";

/// Version of the export layout. Documented in `docs/household-export.md`.
pub const EXPORT_FORMAT_VERSION: i64 = 1;

/// Errors produced by the household export and delete operations.
#[derive(Debug, thiserror::Error)]
pub enum HouseholdDataError {
    /// No household exists with the requested id (a typo, or already deleted).
    #[error("no household with id {0}")]
    NoSuchHousehold(String),

    /// A query or transaction failed at the sqlx layer.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Build the full export document for `household_id`.
///
/// Errors with [`HouseholdDataError::NoSuchHousehold`] if the household does not
/// exist, so a typo in the id never yields a well-formed empty document that
/// looks like a real export.
pub async fn export_household(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<Value, HouseholdDataError> {
    let household = fetch_rows_bound(
        pool,
        "SELECT id, name, created_at FROM households WHERE id = ?1",
        household_id,
    )
    .await?;
    let Some(household) = household.into_iter().next() else {
        return Err(HouseholdDataError::NoSuchHousehold(
            household_id.to_string(),
        ));
    };

    let members = fetch_rows_bound(
        pool,
        "SELECT hm.user_id, u.display_name, hm.role, hm.joined_at \
         FROM household_members hm JOIN users u ON u.id = hm.user_id \
         WHERE hm.household_id = ?1 ORDER BY hm.user_id",
        household_id,
    )
    .await?;

    // Shared reference data the household's records point at, so the document
    // can be read without a BuyWise database. Read whole: categories and
    // subcategories are a small, closed vocabulary.
    let categories = fetch_rows(
        pool,
        "SELECT id, name_en, name_es, icon, default_order, is_fresh \
         FROM categories ORDER BY id",
    )
    .await?;
    let subcategories = fetch_rows(
        pool,
        "SELECT id, category_id, name_en, name_es, default_order \
         FROM subcategories ORDER BY id",
    )
    .await?;

    let catalogue_items = fetch_rows_bound(
        pool,
        "SELECT id, name_en, name_es, category_id, subcategory_id, \
                default_constraint_policy, created_at \
         FROM canonical_items WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    let aliases = fetch_rows_bound(
        pool,
        "SELECT id, canonical_item_id, alias, language, source, created_at \
         FROM item_aliases WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    let constraints = fetch_rows_bound(
        pool,
        "SELECT c.id, c.canonical_item_id, c.brand, c.allowed_brands, \
                c.excluded_brands, c.preferred_size_value, c.preferred_size_basis, \
                c.min_size_value, c.min_size_basis, c.max_size_value, c.max_size_basis, \
                c.pack_count, c.form_factor, c.dietary_flags, c.substitution_policy, \
                c.updated_at \
         FROM item_constraints c JOIN canonical_items ci ON ci.id = c.canonical_item_id \
         WHERE ci.household_id = ?1 ORDER BY c.id",
        household_id,
    )
    .await?;

    let preferences = json!({
        "settings": fetch_rows_bound(
            pool,
            "SELECT convenience_threshold_2_cents, convenience_threshold_3_cents, updated_at \
             FROM household_settings WHERE household_id = ?1",
            household_id,
        ).await?.into_iter().next().unwrap_or(Value::Null),
        "stores": fetch_rows_bound(
            pool,
            "SELECT retailer, status, nearby, updated_at \
             FROM household_store_settings WHERE household_id = ?1 ORDER BY retailer",
            household_id,
        ).await?,
        "store_category_rules": fetch_rows_bound(
            pool,
            "SELECT retailer, category_id, subcategory_id, allowed, updated_at \
             FROM household_store_category_rules WHERE household_id = ?1 \
             ORDER BY retailer, category_id, subcategory_id",
            household_id,
        ).await?,
        "store_category_orders": fetch_rows_bound(
            pool,
            "SELECT store, category_id, display_order, updated_at \
             FROM store_category_orders WHERE household_id = ?1 \
             ORDER BY store, display_order, category_id",
            household_id,
        ).await?,
        "purchase_dismissals": fetch_rows_bound(
            pool,
            "SELECT id, canonical_item_id, dismissed_by, dismissed_at \
             FROM purchase_dismissals WHERE household_id = ?1 ORDER BY id",
            household_id,
        ).await?,
    });

    let product_matches = fetch_rows_bound(
        pool,
        "SELECT id, canonical_item_id, retailer_product_id, status, matched_by, \
                created_at, updated_at \
         FROM product_matches WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    // Only the retailer products this household's matches point at, so the
    // document stays bounded by household size rather than the shared price
    // catalogue.
    let retailer_products = fetch_rows_bound(
        pool,
        "SELECT id, source, retailer, external_id, url, name, brand, category, \
                pack_size_text, size_value, size_unit, pack_count, form_factor, \
                dietary_flags, first_seen_at, last_seen_at \
         FROM retailer_products WHERE id IN \
             (SELECT retailer_product_id FROM product_matches WHERE household_id = ?1) \
         ORDER BY id",
        household_id,
    )
    .await?;
    let match_feedback = fetch_rows_bound(
        pool,
        "SELECT mf.id, mf.product_match_id, mf.user_id, mf.action, mf.created_at \
         FROM match_feedback mf JOIN product_matches pm ON pm.id = mf.product_match_id \
         WHERE pm.household_id = ?1 ORDER BY mf.id",
        household_id,
    )
    .await?;

    let purchase_events = fetch_rows_bound(
        pool,
        "SELECT id, canonical_item_id, list_id, entry_id, source, occurred_at, recorded_at \
         FROM purchase_events WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;

    let mut lists = fetch_rows_bound(
        pool,
        "SELECT id, name, status, current_revision, created_at, archived_at \
         FROM shopping_lists WHERE household_id = ?1 ORDER BY id",
        household_id,
    )
    .await?;
    attach_children(
        &mut lists,
        "entries",
        fetch_rows_bound(
            pool,
            "SELECT le.id, le.list_id, le.canonical_item_id, le.resolution_status, \
                    le.resolution_candidates_json, le.raw_text, le.display_name, \
                    le.added_by, le.quantity, le.status, le.bought_by, le.bought_at, \
                    le.effective_constraint_hash, le.last_updated_revision, \
                    le.created_at, le.updated_at \
             FROM list_entries le JOIN shopping_lists sl ON sl.id = le.list_id \
             WHERE sl.household_id = ?1 ORDER BY le.id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "entry_events",
        fetch_rows_bound(
            pool,
            "SELECT e.id, e.entry_id, le.list_id, e.user_id, e.event_type, e.payload, e.created_at \
             FROM list_entry_events e \
             JOIN list_entries le ON le.id = e.entry_id \
             JOIN shopping_lists sl ON sl.id = le.list_id \
             WHERE sl.household_id = ?1 ORDER BY e.id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "mutations",
        fetch_rows_bound(
            pool,
            "SELECT id, client_mutation_id, device_id, user_id, list_id, entity_type, \
                    entity_id, operation_type, payload, base_revision, created_at_client, \
                    received_at_server, server_revision, status, response_json \
             FROM mutations WHERE list_id IN \
                 (SELECT id FROM shopping_lists WHERE household_id = ?1) ORDER BY id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "recommendation_runs",
        fetch_rows_bound(
            pool,
            "SELECT id, list_id, run_at, offer_snapshot_date, plans_json, recommended_plan, \
                    convenience_threshold_2, convenience_threshold_3 \
             FROM recommendation_runs WHERE household_id = ?1 ORDER BY id",
            household_id,
        )
        .await?,
    );
    attach_children(
        &mut lists,
        "savings_events",
        fetch_rows_bound(
            pool,
            "SELECT s.id, s.list_id, s.recommendation_run_id, s.baseline_cost_cents, \
                    s.recommended_cost_cents, s.theoretical_minimum_cost_cents, \
                    s.chosen_basket_cost_cents, s.chosen_plan, s.chosen_stores, \
                    s.gross_saving_cents, s.accepted_savings_cents, s.rejected_savings_cents, \
                    s.travel_cost_cents, s.subscription_cost_alloc_cents, s.net_saving_cents, \
                    s.status, s.created_at \
             FROM savings_events s JOIN shopping_lists sl ON sl.id = s.list_id \
             WHERE sl.household_id = ?1 ORDER BY s.id",
            household_id,
        )
        .await?,
    );

    let mut catalogue = nest_children(catalogue_items, &aliases, "canonical_item_id", "aliases");
    let mut constraints_by_item = group_by(constraints, "canonical_item_id");
    for item in catalogue.iter_mut() {
        if let Value::Object(map) = item {
            let id = map.get("id").and_then(Value::as_str).unwrap_or_default();
            let constraint = constraints_by_item
                .remove(id)
                .and_then(|mut rows| rows.pop())
                .unwrap_or(Value::Null);
            map.insert("constraint".to_string(), constraint);
        }
    }

    Ok(json!({
        "format": EXPORT_FORMAT,
        "format_version": EXPORT_FORMAT_VERSION,
        "exported_at": db::now_iso(),
        "household": household,
        "members": members,
        "reference_data": { "categories": categories, "subcategories": subcategories },
        "catalogue": catalogue,
        "preferences": preferences,
        "product_matches": product_matches,
        "retailer_products": retailer_products,
        "match_feedback": match_feedback,
        "lists": lists,
        "purchase_events": purchase_events,
    }))
}

/// Attach `children` (grouped by their `list_id`) to each list under `key`.
fn attach_children(lists: &mut [Value], key: &str, children: Vec<Value>) {
    let mut grouped = group_by(children, "list_id");
    for list in lists.iter_mut() {
        if let Value::Object(map) = list {
            let id = map
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let rows = grouped.remove(&id).unwrap_or_default();
            map.insert(key.to_string(), Value::Array(rows));
        }
    }
}

/// Group `children` by the string value of `parent_key`, preserving order.
fn group_by(children: Vec<Value>, parent_key: &str) -> HashMap<String, Vec<Value>> {
    let mut grouped: HashMap<String, Vec<Value>> = HashMap::new();
    for child in children {
        let Some(parent) = child.get(parent_key).and_then(Value::as_str) else {
            continue;
        };
        grouped.entry(parent.to_string()).or_default().push(child);
    }
    grouped
}

/// Nest each parent's children under `child_key`, keeping child order.
fn nest_children(
    parents: Vec<Value>,
    children: &[Value],
    parent_key: &str,
    child_key: &str,
) -> Vec<Value> {
    let mut grouped = group_by(children.to_vec(), parent_key);
    parents
        .into_iter()
        .map(|mut parent| {
            if let Value::Object(map) = &mut parent {
                let id = map
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let rows = grouped.remove(&id).unwrap_or_default();
                map.insert(child_key.to_string(), Value::Array(rows));
            }
            parent
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Every table row the delete removes for the target household, in dependency
/// order (children before parents, so immediate foreign keys hold).
///
/// Verification guard: `docs/data-flows.md` (Retention and deletion) lists this
/// set; `crates/cli/tests/household_data.rs` asserts a deleted household leaves
/// no row in any of them. `dry_run` executes every statement and rolls back, so
/// the reported counts are exactly what a real run removes.
///
/// Deliberately absent (shared, not household-identifiable): `categories`,
/// `subcategories`, `retailer_products`, `product_offers`, `source_runs`,
/// `api_usage_events`.
pub const HOUSEHOLD_TABLES: &[&str] = &[
    "list_entry_events",
    "savings_events",
    "purchase_events",
    "mutations",
    "list_entries",
    "recommendation_runs",
    "shopping_lists",
    "match_feedback",
    "product_matches",
    "item_constraints",
    "item_aliases",
    "purchase_dismissals",
    "canonical_items",
    "household_store_category_rules",
    "household_store_settings",
    "household_settings",
    "store_category_orders",
    "household_members",
    "households",
    "sessions",
    "users",
];

/// One delete step: the table it clears and the statement that clears it. The
/// label MUST match the corresponding entry in [`HOUSEHOLD_TABLES`] (both are
/// asserted against the same order in the tests).
struct DeleteStep {
    table: &'static str,
    delete_sql: &'static str,
}

/// The delete statements, in dependency order. Every statement binds `?1` to the
/// household id (directly or through a subquery). The `sessions`/`users` steps
/// run last and use the member ids captured before the membership rows are
/// removed, so unrelated membership-less accounts are not affected.
const DELETE_STEPS: &[DeleteStep] = &[
    DeleteStep {
        table: "list_entry_events",
        delete_sql: "DELETE FROM list_entry_events WHERE entry_id IN \
             (SELECT le.id FROM list_entries le \
              JOIN shopping_lists sl ON sl.id = le.list_id WHERE sl.household_id = ?1)",
    },
    DeleteStep {
        table: "savings_events",
        delete_sql: "DELETE FROM savings_events WHERE list_id IN \
             (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "purchase_events",
        delete_sql: "DELETE FROM purchase_events WHERE household_id = ?1",
    },
    DeleteStep {
        table: "mutations",
        delete_sql: "DELETE FROM mutations WHERE list_id IN \
             (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "list_entries",
        delete_sql: "DELETE FROM list_entries WHERE list_id IN \
             (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "recommendation_runs",
        delete_sql: "DELETE FROM recommendation_runs WHERE household_id = ?1",
    },
    DeleteStep {
        table: "shopping_lists",
        delete_sql: "DELETE FROM shopping_lists WHERE household_id = ?1",
    },
    DeleteStep {
        table: "match_feedback",
        delete_sql: "DELETE FROM match_feedback WHERE product_match_id IN \
             (SELECT id FROM product_matches WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "product_matches",
        delete_sql: "DELETE FROM product_matches WHERE household_id = ?1",
    },
    DeleteStep {
        table: "item_constraints",
        delete_sql: "DELETE FROM item_constraints WHERE canonical_item_id IN \
             (SELECT id FROM canonical_items WHERE household_id = ?1)",
    },
    DeleteStep {
        table: "item_aliases",
        delete_sql: "DELETE FROM item_aliases WHERE household_id = ?1",
    },
    DeleteStep {
        table: "purchase_dismissals",
        delete_sql: "DELETE FROM purchase_dismissals WHERE household_id = ?1",
    },
    DeleteStep {
        table: "canonical_items",
        delete_sql: "DELETE FROM canonical_items WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_store_category_rules",
        delete_sql: "DELETE FROM household_store_category_rules WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_store_settings",
        delete_sql: "DELETE FROM household_store_settings WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_settings",
        delete_sql: "DELETE FROM household_settings WHERE household_id = ?1",
    },
    DeleteStep {
        table: "store_category_orders",
        delete_sql: "DELETE FROM store_category_orders WHERE household_id = ?1",
    },
    DeleteStep {
        table: "household_members",
        delete_sql: "DELETE FROM household_members WHERE household_id = ?1",
    },
    DeleteStep {
        table: "households",
        delete_sql: "DELETE FROM households WHERE id = ?1",
    },
    DeleteStep {
        table: "sessions",
        delete_sql: "DELETE FROM sessions WHERE user_id IN \
             (SELECT user_id FROM household_delete_candidates WHERE household_id = ?1 \
              AND NOT EXISTS (SELECT 1 FROM household_members m \
                              WHERE m.user_id = household_delete_candidates.user_id))",
    },
    DeleteStep {
        table: "users",
        delete_sql: "DELETE FROM users WHERE id IN \
             (SELECT user_id FROM household_delete_candidates WHERE household_id = ?1 \
              AND NOT EXISTS (SELECT 1 FROM household_members m \
                              WHERE m.user_id = household_delete_candidates.user_id))",
    },
];

/// Delete every household-scoped row for `household_id` in one transaction.
///
/// With `dry_run`, the same statements run and the transaction is rolled back,
/// so the returned `(table, rows_affected)` pairs are exactly what a real run
/// would remove. Errors with [`HouseholdDataError::NoSuchHousehold`] if the
/// household does not exist.
pub async fn delete_household(
    pool: &SqlitePool,
    household_id: &str,
    dry_run: bool,
) -> Result<Vec<(&'static str, u64)>, HouseholdDataError> {
    household_name(pool, household_id).await?;

    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TEMP TABLE household_delete_candidates \
         (household_id TEXT NOT NULL, user_id TEXT NOT NULL, PRIMARY KEY (household_id, user_id))",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO household_delete_candidates (household_id, user_id) \
         SELECT household_id, user_id FROM household_members WHERE household_id = ?1",
    )
    .bind(household_id)
    .execute(&mut *tx)
    .await?;

    let mut counts = Vec::with_capacity(DELETE_STEPS.len());
    for step in DELETE_STEPS {
        let result = sqlx::query(step.delete_sql)
            .bind(household_id)
            .execute(&mut *tx)
            .await?;
        counts.push((step.table, result.rows_affected()));
    }
    sqlx::query("DROP TABLE household_delete_candidates")
        .execute(&mut *tx)
        .await?;

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(counts)
}

/// The household's display name, or [`HouseholdDataError::NoSuchHousehold`] if
/// no such household exists.
pub async fn household_name(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<String, HouseholdDataError> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM households WHERE id = ?1")
        .bind(household_id)
        .fetch_optional(pool)
        .await?;
    name.ok_or_else(|| HouseholdDataError::NoSuchHousehold(household_id.to_string()))
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Fetch rows as JSON objects, binding `?1` to `household_id`.
async fn fetch_rows_bound(
    pool: &SqlitePool,
    sql: &str,
    household_id: &str,
) -> Result<Vec<Value>, HouseholdDataError> {
    let rows = sqlx::query(sql).bind(household_id).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_object).collect())
}

/// Fetch rows as JSON objects from a statement that binds no parameters.
async fn fetch_rows(pool: &SqlitePool, sql: &str) -> Result<Vec<Value>, HouseholdDataError> {
    let rows = sqlx::query(sql).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_object).collect())
}

/// Convert one SQLite row into a JSON object keyed by column name.
///
/// SQLite is dynamically typed, so the value is decoded by trying the column's
/// expected JSON shapes in order (integer, then real, then text). A NULL — or a
/// stored type none of the three accept — becomes JSON `null`, which keeps the
/// export total rather than failing the whole document on one odd value.
fn row_to_object(row: &SqliteRow) -> Value {
    let mut map = Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        map.insert(column.name().to_string(), column_value(row, index));
    }
    Value::Object(map)
}

fn column_value(row: &SqliteRow, index: usize) -> Value {
    if let Ok(Some(value)) = row.try_get::<Option<i64>, _>(index) {
        return Value::from(value);
    }
    if let Ok(Some(value)) = row.try_get::<Option<f64>, _>(index) {
        return Value::from(value);
    }
    if let Ok(Some(value)) = row.try_get::<Option<String>, _>(index) {
        return Value::from(value);
    }
    Value::Null
}
