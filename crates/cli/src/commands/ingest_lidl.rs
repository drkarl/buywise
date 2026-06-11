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
//! returns [`buywise_ingest::error::IngestError::MissingFeedUrl`], which this
//! command surfaces with an operator-facing `.context(...)` so the CLI prints a
//! clear instruction to set `LIDL_FEED_URL`. That detailed hint is attached
//! **only** to the missing-URL variant; any other failure (a malformed
//! `fixtures/lidl_offers.json` during a dry-run, a DB error, …) gets a neutral
//! `"Lidl ingest"` context, so operators are never told to re-run with the mode
//! that just failed.
//!
//! Thread-safety caveat: `--dry-run` mutates a process-global env var because the
//! Lidl adapter selects its fetch strategy from `BUYWISE_NO_NETWORK`
//! ([`lidl::ingest`] takes only a pool, with no `no_network` parameter). The
//! mutation is **scoped**: a [`NoNetworkGuard`]
//! saves the variable's prior value before setting `BUYWISE_NO_NETWORK=1` and
//! restores it (to the old value, or unset if it was absent) when the run
//! completes — on both the success and error paths. So a dry-run no longer
//! permanently leaks state, and an in-process caller can safely run a dry-run
//! followed by a live ingest. Concurrent calls in the *same* process still race
//! on the shared variable for the duration of a single run, so in-process tests
//! that interleave dry-run and live calls (including the BasketWatch ones, which
//! mutate the *same* variable) should still serialise themselves under a shared
//! guard. Threading a `no_network: bool` through the adapter API would remove the
//! remaining caveat but is out of scope for this CLI change.

use anyhow::{Context, Result};
use buywise_core::db;
use buywise_ingest::error::IngestError;
use buywise_ingest::lidl;

use super::no_network::NoNetworkGuard;

/// Operator-facing hint attached only when live mode fails because
/// `LIDL_FEED_URL` is unset ([`IngestError::MissingFeedUrl`]).
const MISSING_FEED_URL_HINT: &str =
    "Lidl ingest (live mode requires LIDL_FEED_URL to be set to the Lidl feed endpoint; \
     use --dry-run to read fixtures/lidl_offers.json offline)";

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

    // Inspect the `IngestError` before converting to anyhow so the detailed
    // `LIDL_FEED_URL` / `--dry-run` hint is attached *only* to the missing-URL
    // variant. Every other failure (a malformed fixture during a dry-run, a DB
    // error, …) gets a neutral "Lidl ingest" context — matching the sibling
    // BasketWatch command — so we never advise re-running with the very mode that
    // just failed. anyhow's underlying cause chain is preserved in both arms.
    let summary = lidl::ingest(&pool).await.map_err(|err| match err {
        IngestError::MissingFeedUrl => anyhow::Error::new(err).context(MISSING_FEED_URL_HINT),
        other => anyhow::Error::new(other).context("Lidl ingest"),
    })?;

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
