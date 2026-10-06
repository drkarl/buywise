//! Receipt-image maintenance commands (issue #149, roadmap #114).
//!
//! One operator subcommand, run daily by the `buywise-expire-receipts.timer`
//! systemd timer deployed by `deploy/setup.sh`:
//!
//! * `expire-receipts --db <db> [--receipt-dir <dir>]` — delete every receipt
//!   image whose 30-day expiry has passed, file and record, and print a summary.
//!
//! It is safe to run repeatedly and safe on a database restored without its
//! images: a missing file is counted, not an error, and a record is only removed
//! after its file is gone. The room's policy and retention window live in
//! `docs/data-flows.md` §4; the storage and expiry logic lives in
//! [`buywise_core::receipts`], shared with the API's upload path so the two
//! cannot drift.
//!
//! The command names its database as a filesystem path (`--db`, the CLI
//! convention), and the receipt directory defaults to `RECEIPT_DIR` or a
//! `receipts/` directory beside that database file. Like the other data
//! commands it does not run migrations and refuses a database that does not
//! exist, so a typo cannot create an empty database and report a clean sweep.

use std::path::Path;

use anyhow::{bail, Context, Result};
use buywise_core::db;
use buywise_core::receipts;

/// Entry point for `buywise-cli expire-receipts`.
pub fn expire_run(db_path: &str, receipt_dir: Option<&str>) -> Result<()> {
    // Checked before the runtime is built so a bad path fails without spinning
    // up a reactor.
    require_existing_db(db_path)?;
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(expire_run_async(db_path, receipt_dir))
}

async fn expire_run_async(db_path: &str, receipt_dir: Option<&str>) -> Result<()> {
    let pool = connect(db_path).await?;
    let config = receipts::ReceiptConfig::for_database(db_path, receipt_dir);
    let report = receipts::expire_receipts(&pool, &config.dir, chrono::Utc::now())
        .await
        .with_context(|| format!("sweeping expired receipts in {}", config.dir.display()))?;

    println!(
        "receipts: {} expired record(s); {} file(s) removed, {} already gone; {} record(s) removed",
        report.expired_records,
        report.files_removed,
        report.files_already_gone,
        report.records_removed,
    );
    Ok(())
}

/// Refuse to run against a database path that does not exist, so a typo cannot
/// create (via `create_if_missing`) an empty database that reports a clean sweep.
fn require_existing_db(db_path: &str) -> Result<()> {
    if !Path::new(db_path).exists() {
        bail!("database {db_path} does not exist");
    }
    Ok(())
}

/// Open the target database. The sweep does not run migrations: it is a data
/// operation on a database the API has already migrated.
async fn connect(db_path: &str) -> Result<sqlx::SqlitePool> {
    db::connect(&format!("sqlite://{db_path}"))
        .await
        .with_context(|| format!("opening database {db_path}"))
}
