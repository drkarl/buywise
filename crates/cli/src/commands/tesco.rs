//! Tesco Clubcard export import command (issue #107, roadmap #114).
//!
//! `import-tesco --db <db> --household-id <uuid> --file <export.json>` reads one
//! Tesco Clubcard purchase-history export and imports it into the household's
//! existing purchase ledger, matching each line name with the Phase-0 cascade
//! and recording frequency evidence for the matched lines. The parsing,
//! mapping, matching and idempotency rules all live in
//! [`buywise_core::tesco_import`], shared with the ledger so no second store is
//! created; the ledger format is documented in `docs/purchase-ledger.md`.
//!
//! Like the other operator commands it does not run migrations and requires the
//! target database file to already exist, so a typo in `--db` fails loudly. The
//! export is read-only and never copied into the repository; only aggregate
//! counts are printed — never item names, addresses or phone numbers.

use std::path::Path;

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::tesco_import;

/// Entry point for `buywise-cli import-tesco`.
pub fn import_run(db_path: &str, household_id: &str, file: &str) -> Result<()> {
    // Checked before the runtime is built so a bad path fails without spinning
    // up a reactor.
    require_existing_db(db_path)?;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading Tesco export {file}"))?;
    let document: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing Tesco export {file} as JSON"))?;

    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(import_run_async(db_path, household_id, &document))
}

async fn import_run_async(
    db_path: &str,
    household_id: &str,
    document: &serde_json::Value,
) -> Result<()> {
    let pool = connect(db_path).await?;
    let outcome =
        tesco_import::import_tesco_export(&pool, household_id, document, chrono::Utc::now())
            .await
            .context("importing Tesco export")?;

    println!(
        "tesco: {} purchase(s) — inserted {}, updated {}, unchanged {}",
        outcome.purchases, outcome.inserted, outcome.updated, outcome.unchanged
    );
    println!(
        "  lines: {} (matched {}, unmatched {})",
        outcome.lines, outcome.matched_lines, outcome.unmatched_lines
    );
    println!(
        "  frequency evidence: {} event(s) recorded",
        outcome.events_recorded
    );
    if let (Some(first), Some(last)) = (&outcome.first_date, &outcome.last_date) {
        println!("  date range: {first} .. {last}");
    }
    Ok(())
}

/// Refuse to run against a database path that does not exist, so a typo cannot
/// create (via `create_if_missing`) an empty database and silently import
/// nothing.
fn require_existing_db(db_path: &str) -> Result<()> {
    if !Path::new(db_path).exists() {
        bail!("database {db_path} does not exist");
    }
    Ok(())
}

/// Open the target database. The command does not run migrations: it is a data
/// operation on a database the API has already migrated.
async fn connect(db_path: &str) -> Result<sqlx::SqlitePool> {
    db::connect(&format!("sqlite://{db_path}"))
        .await
        .with_context(|| format!("opening database {db_path}"))
}
