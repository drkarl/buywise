//! `ingest-lidl --db <db> [--dry-run]` — run the Lidl direct adapter.
//!
//! Opens (creating if needed) the given SQLite file, migrates it, and runs the
//! Lidl ingest adapter ([`buywise_ingest::lidl::ingest`]). Lidl is the one
//! "direct" retailer (see `README.md`); BasketWatch covers the other four. This
//! is the operator entry point that wires the otherwise-uncalled adapter to a
//! runnable command (issue #90).
//!
//! `--dry-run` sets `BUYWISE_NO_NETWORK=1` internally before the run, so the
//! adapter reads `fixtures/lidl_offers.json` instead of making any live Lidl.ie
//! request — but still writes the `source_runs` + `retailer_products` +
//! `product_offers` rows to the specified database.
//!
//! Live mode reads `LIDL_FEED_URL` for the feed endpoint. There is no verified
//! default Lidl.ie feed (design §16), so an unset value fails fast: the adapter
//! returns [`IngestError::MissingFeedUrl`], which this command surfaces with an
//! operator-facing `.context(...)` so the CLI prints a clear instruction to set
//! `LIDL_FEED_URL`.
//!
//! Thread-safety caveat: `--dry-run` mutates a process-global env var because the
//! Lidl adapter selects its fetch strategy from `BUYWISE_NO_NETWORK`
//! ([`lidl::ingest`] takes only a pool, with no `no_network` parameter). The
//! mutation is **scoped**: a [`NoNetworkGuard`] saves the variable's prior value
//! before setting `BUYWISE_NO_NETWORK=1` and restores it (to the old value, or
//! unset if it was absent) when the run completes — on both the success and error
//! paths. So a dry-run no longer permanently leaks state, and an in-process caller
//! can safely run a dry-run followed by a live ingest. Concurrent calls in the
//! *same* process still race on the shared variable for the duration of a single
//! run, so in-process tests that interleave dry-run and live calls (including the
//! BasketWatch ones, which mutate the *same* variable) should still serialise
//! themselves under a shared guard. Threading a `no_network: bool` through the
//! adapter API would remove the remaining caveat but is out of scope for this CLI
//! change.

use anyhow::{Context, Result};
use buywise_core::db;
use buywise_ingest::lidl;

const NO_NETWORK_VAR: &str = "BUYWISE_NO_NETWORK";

/// RAII guard that forces the Lidl adapter's no-network path for the lifetime of
/// a dry-run, restoring `BUYWISE_NO_NETWORK` to its prior value (or unsetting it
/// if it was absent) on drop. This keeps the env mutation scoped to the run
/// rather than leaking it process-wide.
struct NoNetworkGuard {
    previous: Option<String>,
}

impl NoNetworkGuard {
    fn engage() -> Self {
        let previous = std::env::var(NO_NETWORK_VAR).ok();
        // SAFETY: mutating the process environment is `unsafe` in edition 2024
        // because it is not thread-safe. The CLI mutates this var only here,
        // and the guard restores the prior value on drop, so the window in
        // which the global is changed is bounded by a single ingest run.
        unsafe {
            std::env::set_var(NO_NETWORK_VAR, "1");
        }
        Self { previous }
    }
}

impl Drop for NoNetworkGuard {
    fn drop(&mut self) {
        // SAFETY: see `engage` — restoring the previous value (or unsetting it)
        // is the matching mutation that keeps the change scoped to this run.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(NO_NETWORK_VAR, value),
                None => std::env::remove_var(NO_NETWORK_VAR),
            }
        }
    }
}

/// Entry point for `buywise-cli ingest-lidl`.
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
        println!("dry-run mode: no network calls (reading fixtures/lidl_offers.json)");
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

    let summary = lidl::ingest(&pool).await.context(
        "Lidl ingest (live mode requires LIDL_FEED_URL to be set to the Lidl feed endpoint; \
         use --dry-run to read fixtures/lidl_offers.json offline)",
    )?;

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
