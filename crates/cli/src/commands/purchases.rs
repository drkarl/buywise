//! Purchase-ledger import command (issue #148, roadmap #114).
//!
//! `import-purchases --db <db> --household-id <uuid> --file <json>` imports one
//! document of purchase transactions into a household's ledger. The parsing,
//! validation, identity/fingerprint and idempotency rules all live in
//! [`buywise_core::purchase_ledger`], shared with the member-facing API
//! (`crates/api/src/purchases.rs`) so the CLI and the API cannot drift; the
//! format is documented in `docs/purchase-ledger.md`. This module is the CLI's
//! thin front end — path validation, a sync entry point, and printed output.
//!
//! Like the other operator commands it does not run migrations and requires the
//! target database file to already exist, so a typo in `--db` fails loudly
//! instead of creating a fresh, unmigrated database. The database is expected to
//! have been migrated by the API (or `bootstrap-household`).

use std::path::Path;

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::purchase_ledger::{self, ImportStatus};

/// Entry point for `buywise-cli import-purchases`.
pub fn import_run(db_path: &str, household_id: &str, file: &str) -> Result<()> {
    // Checked before the runtime is built so a bad path fails without spinning
    // up a reactor.
    require_existing_db(db_path)?;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading purchase import {file}"))?;
    let document: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing purchase import {file} as JSON"))?;

    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(import_run_async(db_path, household_id, &document))
}

async fn import_run_async(
    db_path: &str,
    household_id: &str,
    document: &serde_json::Value,
) -> Result<()> {
    let pool = connect(db_path).await?;
    let outcome = purchase_ledger::import_transactions(&pool, household_id, document).await?;

    println!(
        "household {household_id}: imported {} transaction(s)",
        outcome.results.len()
    );
    for result in &outcome.results {
        println!(
            "  {} revision {} {}",
            result.transaction_id,
            result.revision,
            status_word(result.status)
        );
    }
    println!(
        "  inserted: {}, updated: {}, unchanged: {}",
        outcome.inserted, outcome.updated, outcome.unchanged
    );
    Ok(())
}

/// The past-tense verb for one transaction result.
fn status_word(status: ImportStatus) -> &'static str {
    match status {
        ImportStatus::Inserted => "inserted",
        ImportStatus::Updated => "updated (previous revision kept)",
        ImportStatus::Unchanged => "unchanged",
    }
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
