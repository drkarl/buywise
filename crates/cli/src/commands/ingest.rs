//! `ingest-basketwatch --db <db> [--dry-run]` — run the BasketWatch adapter.
//!
//! Opens (creating if needed) the given SQLite file, migrates it, and runs the
//! BasketWatch ingest adapter ([`buywise_ingest::basketwatch::ingest`]).
//!
//! `--dry-run` sets `BUYWISE_NO_NETWORK=1` internally before the run, so the
//! adapter reads `fixtures/offers.json` instead of making any live RapidAPI
//! calls — but still writes the `source_runs` + `retailer_products` +
//! `product_offers` rows to the specified database (issue #11).

use anyhow::{Context, Result};
use buywise_core::db;
use buywise_ingest::basketwatch;

/// Entry point for `buywise-cli ingest-basketwatch`.
pub fn run(db_path: &str, dry_run: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(db_path, dry_run))
}

async fn run_async(db_path: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        // The adapter selects its fetch strategy from this env var; setting it
        // here is the documented way to force the no-network path from the CLI.
        std::env::set_var("BUYWISE_NO_NETWORK", "1");
        println!("dry-run mode: no network calls (reading fixtures/offers.json)");
    }

    let url = format!("sqlite://{db_path}");
    let pool = db::connect(&url)
        .await
        .with_context(|| format!("opening database {db_path}"))?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    let summary = basketwatch::ingest(&pool)
        .await
        .context("BasketWatch ingest")?;

    println!(
        "ingest complete: run #{} — fetched {}, normalised {}, skipped {}, requests {}",
        summary.run_number,
        summary.items_fetched,
        summary.items_normalised,
        summary.skipped,
        summary.requests_made,
    );
    Ok(())
}
