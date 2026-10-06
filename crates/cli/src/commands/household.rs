//! Household data-controls commands (issue #119, roadmap #114).
//!
//! Two operator subcommands that give a private household control over the
//! records the BuyWise server holds for it:
//!
//! * `export-household --db <db> --household-id <uuid> [--out <path>]` — write
//!   every household-scoped record as one JSON document (receipt-image metadata
//!   included; the image bytes are not records and are not in the document).
//! * `delete-household --db <db> --household-id <uuid> [--confirm] [--dry-run]
//!   [--receipt-dir <dir>]` — remove every household-scoped record in one
//!   transaction, unlinking the household's receipt-image files in the same
//!   transaction. `--dry-run` runs the whole delete inside a transaction and
//!   ROLLS IT BACK, so it reports the exact rows a real run would remove without
//!   guessing at per-table predicates — and removes no file; a real run still
//!   refuses without `--confirm`.
//!
//! The export layout and the delete semantics live in
//! [`buywise_core::household_data`], shared with the member-facing API endpoints
//! (issue #136) so the two never drift; this module is the CLI's thin front end
//! — path validation, a sync entry point, and printed output. The layout is
//! documented in `docs/household-export.md`, and what is and is not
//! household-scoped in `docs/data-flows.md` (Retention and deletion).
//!
//! Both commands are READ-ONLY against the schema (neither runs migrations) and
//! require the target database file to already exist, so a typo in `--db` fails
//! loudly instead of creating a fresh empty database and silently exporting or
//! deleting nothing.

use std::path::Path;

use anyhow::{bail, Context, Result};
use buywise_core::db;

pub use buywise_core::household_data::{
    delete_household, export_household, household_name, EXPORT_FORMAT, EXPORT_FORMAT_VERSION,
    HOUSEHOLD_TABLES,
};

/// Entry point for `buywise-cli export-household`.
pub fn export_run(db_path: &str, household_id: &str, out: Option<&str>) -> Result<()> {
    // Checked before the runtime is built so a bad path fails without spinning
    // up a reactor.
    require_existing_db(db_path)?;
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(export_run_async(db_path, household_id, out))
}

async fn export_run_async(db_path: &str, household_id: &str, out: Option<&str>) -> Result<()> {
    let pool = connect(db_path).await?;
    let document = export_household(&pool, household_id).await?;
    let json = serde_json::to_string_pretty(&document).context("serialising export")?;

    match out {
        Some(path) => {
            std::fs::write(path, format!("{json}\n"))
                .with_context(|| format!("writing export to {path}"))?;
            println!("exported household {household_id} to {path}");
        }
        None => println!("{json}"),
    }
    Ok(())
}

/// Entry point for `buywise-cli delete-household`.
///
/// Refuses unless `--confirm` is given (a real delete) or `--dry-run` is given,
/// so the command is never destructive by accident. `receipt_dir_override` is
/// `--receipt-dir`; the resolved directory (that override, else `RECEIPT_DIR`,
/// else beside the database) holds the household's receipt images, whose files
/// are removed with their records.
pub fn delete_run(
    db_path: &str,
    household_id: &str,
    confirm: bool,
    dry_run: bool,
    receipt_dir_override: Option<&str>,
) -> Result<()> {
    if !confirm && !dry_run {
        bail!(
            "refusing to delete household {household_id}: pass --confirm to delete, \
             or --dry-run to see what would be removed"
        );
    }
    // Checked before the runtime is built so a bad path fails without spinning
    // up a reactor.
    require_existing_db(db_path)?;
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(delete_run_async(
        db_path,
        household_id,
        dry_run,
        receipt_dir_override,
    ))
}

async fn delete_run_async(
    db_path: &str,
    household_id: &str,
    dry_run: bool,
    receipt_dir_override: Option<&str>,
) -> Result<()> {
    let pool = connect(db_path).await?;
    let receipt_config =
        buywise_core::receipts::ReceiptConfig::for_database(db_path, receipt_dir_override);
    let household = household_name(&pool, household_id).await?;
    let counts = delete_household(&pool, household_id, dry_run, &receipt_config.dir).await?;

    let verb = if dry_run { "would delete" } else { "deleted" };
    println!("household {household_id} ({household}): {verb} rows");
    let mut total = 0u64;
    for (table, count) in &counts {
        total += count;
        if *count > 0 {
            println!("  {table}: {count}");
        }
    }
    println!("  total: {total}");
    if dry_run {
        println!("dry run: nothing was deleted (transaction rolled back)");
    }
    Ok(())
}

/// Refuse to run against a database path that does not exist, so a typo cannot
/// create (via `create_if_missing`) an empty database that silently exports or
/// deletes nothing.
fn require_existing_db(db_path: &str) -> Result<()> {
    if !Path::new(db_path).exists() {
        bail!("database {db_path} does not exist");
    }
    Ok(())
}

/// Open the target database. Neither command runs migrations: export must not
/// mutate, and delete is a data operation on a database the API has already
/// migrated.
async fn connect(db_path: &str) -> Result<sqlx::SqlitePool> {
    db::connect(&format!("sqlite://{db_path}"))
        .await
        .with_context(|| format!("opening database {db_path}"))
}
