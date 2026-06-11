//! `ingest-basketwatch --db <db> [--dry-run]` — run the BasketWatch adapter.
//!
//! Opens (creating if needed) the given SQLite file, migrates it, and runs the
//! BasketWatch ingest adapter ([`buywise_ingest::basketwatch::ingest`]).
//!
//! `--dry-run` sets `BUYWISE_NO_NETWORK=1` internally before the run, so the
//! adapter reads `fixtures/offers.json` instead of making any live RapidAPI
//! calls — but still writes the `source_runs` + `retailer_products` +
//! `product_offers` rows to the specified database (issue #11).
//!
//! Thread-safety caveat: `--dry-run` mutates a process-global env var because the
//! BasketWatch adapter selects its fetch strategy from `BUYWISE_NO_NETWORK`
//! ([`basketwatch::ingest`] takes only a pool, with no `no_network` parameter).
//! The mutation is now **scoped**: a [`NoNetworkGuard`]
//! saves the variable's prior value before setting `BUYWISE_NO_NETWORK=1` and
//! restores it (to the old value, or unset if it was absent) when the run
//! completes — on both the success and error paths. So a dry-run no longer
//! permanently leaks state, and an in-process caller can safely run a dry-run
//! followed by a live ingest. Concurrent calls in the *same* process still race
//! on the shared variable for the duration of a single run, so in-process tests
//! that interleave dry-run and live calls should still serialise themselves under
//! a shared guard. Threading a `no_network: bool` through the adapter API would
//! remove the remaining caveat but is out of scope for this CLI change.

use anyhow::{Context, Result};
use buywise_core::db;
use buywise_ingest::basketwatch;

use super::no_network::NoNetworkGuard;

/// Entry point for `buywise-cli ingest-basketwatch`.
pub fn run(db_path: &str, dry_run: bool) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    runtime.block_on(run_async(db_path, dry_run))
}

async fn run_async(db_path: &str, dry_run: bool) -> Result<()> {
    // Held for the whole run so the no-network env var is restored to its prior
    // value on every return path (including errors via `?`) when the guard drops.
    let _no_network_guard = if dry_run {
        // The adapter selects its fetch strategy from this env var; forcing it
        // here (scoped to the guard) is the documented no-network path.
        let guard = NoNetworkGuard::engage();
        println!("dry-run mode: no network calls (reading fixtures/offers.json)");
        Some(guard)
    } else {
        None
    };

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
