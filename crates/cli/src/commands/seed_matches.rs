//! `seed-matches --from <csv> --db <db> --household-id <uuid>`.
//!
//! Upserts `product_matches` rows from a CSV with columns
//! `(canonical_item_id, retailer_product_id, status, matched_by)`.
//!
//! Upserts use `ON CONFLICT (household_id, canonical_item_id, retailer_product_id)
//! DO UPDATE` — **never** `INSERT OR REPLACE`. REPLACE is a DELETE + INSERT that
//! would mint a new `id`, breaking any future `match_feedback` foreign keys that
//! reference the existing match row (issue #11). `DO UPDATE` preserves the
//! stable primary key, so a second run is idempotent (only `status`/`matched_by`
//! /`updated_at` change, and only if the input changed).
//!
//! `--household-id` is required (Phase 0:
//! `00000000-0000-0000-0000-000000000001`).

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::MatchStatus;
use sqlx::sqlite::SqliteConnection;

/// One parsed CSV row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvMatch {
    pub canonical_item_id: String,
    pub retailer_product_id: String,
    /// Match status DB string (`accepted`/`rejected`/`suggested`/`pending`).
    pub status: String,
    /// Who created the match DB string (`user`/`system`/`llm`).
    pub matched_by: String,
}

/// Entry point for `buywise-cli seed-matches`.
pub fn run(csv_path: &str, db_path: &str, household_id: &str) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(csv_path, db_path, household_id))
}

async fn run_async(csv_path: &str, db_path: &str, household_id: &str) -> Result<()> {
    let raw = std::fs::read_to_string(csv_path).with_context(|| format!("reading {csv_path}"))?;
    let matches = parse_matches_csv(&raw).with_context(|| format!("parsing {csv_path}"))?;

    let url = format!("sqlite://{db_path}");
    let pool = db::connect(&url)
        .await
        .with_context(|| format!("opening database {db_path}"))?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    let now = db::now_iso();
    // Wrap the whole batch in one transaction so a mid-file failure (e.g. an
    // unknown retailer_product_id on row N) rolls back every prior upsert and
    // never leaves the table half-populated.
    let mut tx = pool.begin().await.context("beginning transaction")?;
    let mut upserted = 0usize;
    for m in &matches {
        // `&mut tx` auto-derefs the transaction to its underlying connection so
        // each row's FK-guard read and the upsert write share one atomic unit.
        upsert_match(&mut tx, household_id, m, &now).await?;
        upserted += 1;
    }
    tx.commit().await.context("committing transaction")?;

    println!("seeded {upserted} product_matches into {db_path} (idempotent upsert)");
    Ok(())
}

/// Parse the `product_matches.csv` body into rows.
///
/// A minimal, dependency-free parser sufficient for the controlled fixture
/// format: a header line (validated by name) followed by comma-separated rows
/// with no embedded commas, quotes, or newlines inside fields. Blank lines and
/// lines beginning with `#` (comments) are skipped. Validates the status and
/// matched_by enums against the schema CHECK so a bad value fails here rather
/// than at INSERT time.
pub fn parse_matches_csv(body: &str) -> Result<Vec<CsvMatch>> {
    const EXPECTED_HEADER: [&str; 4] = [
        "canonical_item_id",
        "retailer_product_id",
        "status",
        "matched_by",
    ];

    let mut lines = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));

    let header = lines.next().context("CSV is empty (no header row)")?;
    let header_cols: Vec<&str> = header.split(',').map(str::trim).collect();
    if header_cols != EXPECTED_HEADER {
        bail!(
            "unexpected CSV header {:?}; expected {:?}",
            header_cols,
            EXPECTED_HEADER
        );
    }

    let mut out = Vec::new();
    for (i, line) in lines.enumerate() {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() != 4 {
            bail!(
                "CSV row {} has {} columns, expected 4: {line:?}",
                i + 1,
                cols.len()
            );
        }
        let (canonical_item_id, retailer_product_id, status, matched_by) =
            (cols[0], cols[1], cols[2], cols[3]);

        // Validate enums up front against the schema CHECK constraints.
        MatchStatus::from_db_str(status)
            .with_context(|| format!("CSV row {} has invalid status {status:?}", i + 1))?;
        if !matches!(matched_by, "user" | "system" | "llm") {
            bail!(
                "CSV row {} has invalid matched_by {matched_by:?} (expected user|system|llm)",
                i + 1
            );
        }
        if canonical_item_id.is_empty() || retailer_product_id.is_empty() {
            bail!("CSV row {} has an empty id field: {line:?}", i + 1);
        }

        out.push(CsvMatch {
            canonical_item_id: canonical_item_id.to_string(),
            retailer_product_id: retailer_product_id.to_string(),
            status: status.to_string(),
            matched_by: matched_by.to_string(),
        });
    }
    Ok(out)
}

/// Upsert a single match row within an open transaction. The `id` is a fresh
/// UUID on first insert and preserved on conflict (see the module doc on why
/// REPLACE is wrong). On conflict only the mutable columns are updated.
///
/// Takes `&mut SqliteConnection` (the transaction auto-derefs to it) so the
/// FK-guard reads and the upsert write all run inside the caller's transaction;
/// a `bail!` here propagates and rolls the whole batch back.
async fn upsert_match(
    conn: &mut SqliteConnection,
    household_id: &str,
    m: &CsvMatch,
    now: &str,
) -> Result<()> {
    // Guard the FKs with a clear message: the schema FKs would otherwise surface
    // a generic constraint error.
    let known_product: Option<String> =
        sqlx::query_scalar("SELECT id FROM retailer_products WHERE id = ?1")
            .bind(&m.retailer_product_id)
            .fetch_optional(&mut *conn)
            .await?;
    if known_product.is_none() {
        bail!(
            "retailer_product_id {:?} not found — run ingest-basketwatch first",
            m.retailer_product_id
        );
    }
    let known_item: Option<String> =
        sqlx::query_scalar("SELECT id FROM canonical_items WHERE id = ?1")
            .bind(&m.canonical_item_id)
            .fetch_optional(&mut *conn)
            .await?;
    if known_item.is_none() {
        bail!(
            "canonical_item_id {:?} not found — does the database have the 002 seed applied?",
            m.canonical_item_id
        );
    }

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO product_matches \
         (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, \
          created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
         ON CONFLICT(household_id, canonical_item_id, retailer_product_id) \
         DO UPDATE SET status = excluded.status, \
                       matched_by = excluded.matched_by, \
                       updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(household_id)
    .bind(&m.canonical_item_id)
    .bind(&m.retailer_product_id)
    .bind(&m.status)
    .bind(&m.matched_by)
    .bind(now)
    .execute(&mut *conn)
    .await
    .context("upserting product_matches row")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUSEHOLD: &str = "00000000-0000-0000-0000-000000000001";

    fn good_row() -> &'static str {
        "canonical_item_id,retailer_product_id,status,matched_by\nitem1,prod1,accepted,system\n"
    }

    #[test]
    fn parses_valid_rows_skipping_comments_and_blanks() {
        let body = "# a comment\n\ncanonical_item_id,retailer_product_id,status,matched_by\n\
                    item1,prod1,accepted,system\n# trailing comment\nitem2,prod2,pending,llm\n";
        let rows = parse_matches_csv(body).expect("parse");
        assert_eq!(
            rows,
            vec![
                CsvMatch {
                    canonical_item_id: "item1".to_string(),
                    retailer_product_id: "prod1".to_string(),
                    status: "accepted".to_string(),
                    matched_by: "system".to_string(),
                },
                CsvMatch {
                    canonical_item_id: "item2".to_string(),
                    retailer_product_id: "prod2".to_string(),
                    status: "pending".to_string(),
                    matched_by: "llm".to_string(),
                },
            ]
        );
    }

    #[test]
    fn rejects_missing_or_wrong_header() {
        let err = parse_matches_csv("").expect_err("empty");
        assert!(err.to_string().contains("no header"), "got: {err}");

        let body = "a,b,c,d\nitem1,prod1,accepted,system\n";
        let err = parse_matches_csv(body).expect_err("bad header");
        assert!(
            err.to_string().contains("unexpected CSV header"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_wrong_arity() {
        let body =
            "canonical_item_id,retailer_product_id,status,matched_by\nitem1,prod1,accepted\n";
        let err = parse_matches_csv(body).expect_err("too few columns");
        assert!(err.to_string().contains("expected 4"), "got: {err}");
    }

    #[test]
    fn rejects_invalid_status_enum() {
        let body = good_row().replace("accepted", "approved");
        let err = parse_matches_csv(&body).expect_err("bad status");
        assert!(err.to_string().contains("invalid status"), "got: {err}");
    }

    #[test]
    fn rejects_invalid_matched_by_enum() {
        let body = good_row().replace("system", "robot");
        let err = parse_matches_csv(&body).expect_err("bad matched_by");
        assert!(err.to_string().contains("invalid matched_by"), "got: {err}");
    }

    #[test]
    fn rejects_empty_id_field() {
        let body =
            "canonical_item_id,retailer_product_id,status,matched_by\n,prod1,accepted,system\n";
        let err = parse_matches_csv(body).expect_err("empty id");
        assert!(err.to_string().contains("empty id"), "got: {err}");
    }

    // Keep the household constant referenced so it documents the Phase-0 id even
    // though the pure parser tests don't touch the DB.
    #[test]
    fn household_constant_is_the_phase0_dev_id() {
        assert_eq!(HOUSEHOLD, "00000000-0000-0000-0000-000000000001");
    }
}
