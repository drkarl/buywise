//! Lidl.ie direct ingest adapter (P0-T9).
//!
//! Fetches Lidl's own product/price data directly from Lidl.ie and normalises it
//! into `retailer_products` + `product_offers` rows, exactly like the BasketWatch
//! adapter ([`crate::basketwatch`]) but for the single `Source::LidlDirect` /
//! `Retailer::Lidl` pairing. It shares the same `source_runs` run flow, the same
//! `BUYWISE_NO_NETWORK` no-network gating, and the same deterministic
//! [`stable_product_id`] keys.
//!
//! Two things differ from BasketWatch:
//!
//! * **Confidence.** Lidl offers are stamped [`SourceConfidence::Medium`] (the
//!   feed is a single-retailer direct scrape whose long-term reliability is an
//!   open question — design §16), where BasketWatch offers are `High`.
//! * **Unit price / pack size.** Lidl does not supply a pre-classified unit-price
//!   basis; the pack size is embedded in the product *name*. We therefore derive
//!   both via [`unit_price_for_product`] (design §9), which infers the basis from
//!   the name and runs [`parse_pack_size`](buywise_core::normalise::parse_pack_size)
//!   over the name to extract the size.
//!
//! # Run flow (identical in structure to BasketWatch P0-T8)
//!
//! 1. Commit a `source_runs` row (`status = 'running'`) before any network I/O.
//! 2. Fetch (or, under `BUYWISE_NO_NETWORK=1`, read `fixtures/lidl_offers.json`).
//! 3. Normalise the raw payload into in-memory products + offer drafts.
//! 4. On success: `BEGIN IMMEDIATE` → allocate the next run number → upsert
//!    products → insert offers → mark the run `success` → `COMMIT`.
//! 5. On any failure: roll back the write transaction (if open), then mark the
//!    original `running` row `failed` in a short separate transaction.
//!
//! # Isolation from BasketWatch
//!
//! The Lidl feed's reliability is uncertain. A Lidl failure must never block or
//! fail the BasketWatch pipeline, so the orchestration entry point
//! [`spawn_ingest`] runs the whole ingest on a detached task and catches an
//! *inner* `tokio::spawn` so that any error — *or panic* — is logged and
//! swallowed. The caller is handed a
//! [`JoinHandle`](tokio::task::JoinHandle) that always resolves to `Ok(())`: it
//! can await it for observability but never sees a Lidl error or panic. Because
//! Lidl has its own `Source::LidlDirect` run rows and run-number sequence, it is
//! fully independent of the BasketWatch run history.

use std::time::Duration;

use buywise_core::db;
use buywise_core::normalise::unit_price_for_product;
use buywise_core::{Cents, ProductOffer, Retailer, RetailerProduct, Source, SourceConfidence};
use serde::Deserialize;
use sqlx::sqlite::SqlitePool;
use tokio::task::JoinHandle;

use crate::error::IngestError;
use crate::product_id::stable_product_id;

/// Lidl offers carry medium confidence: a single-retailer direct scrape whose
/// long-term reliability is an open question (design §16).
const LIDL_CONFIDENCE: SourceConfidence = SourceConfidence::Medium;

/// Per-request HTTP timeout. Bounds a stalled `send()` so a hung response cannot
/// keep the `running` row open indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One product+offer record as delivered by Lidl.ie (and as stored in
/// `fixtures/lidl_offers.json`).
///
/// `external_id` is `Option` so a missing id can be detected and the row skipped
/// rather than silently inserted — a NULL `external_id` would bypass the partial
/// unique index and defeat deduplication. The pack size is *not* a separate
/// field: Lidl embeds it in `name`, and [`normalise`] extracts it from there.
#[derive(Debug, Clone, Deserialize)]
pub struct RawLidlProduct {
    /// Lidl-assigned product id. Missing/blank ⇒ the row is skipped.
    pub external_id: Option<String>,
    /// Full product name, with the pack size embedded (e.g.
    /// `"Lidl Semi Skimmed Milk 2L"`).
    pub name: String,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Shelf price in cents.
    pub price_cents: Cents,
    #[serde(default)]
    pub previous_price_cents: Option<Cents>,
    #[serde(default)]
    pub offer_text: Option<String>,
    #[serde(default)]
    pub offer_valid_until: Option<String>,
    #[serde(default)]
    pub source_snapshot_date: Option<String>,
    #[serde(default)]
    pub cache_expires_at: Option<String>,
}

/// An offer pending insertion: every `product_offers` field *except* the ones
/// only known inside the success transaction (the offer's own id, the run id,
/// and the run number). Carrying these as drafts keeps [`normalise`] pure and
/// prevents stamping an unallocated run number.
#[derive(Debug, Clone, PartialEq)]
pub struct OfferDraft {
    pub retailer_product_id: String,
    pub price_cents: Cents,
    pub unit_price_eur_micros: Option<i64>,
    pub unit_price_basis: Option<buywise_core::UnitPriceBasis>,
    pub previous_price_cents: Option<Cents>,
    pub offer_text: Option<String>,
    pub offer_valid_until: Option<String>,
    pub source_snapshot_date: Option<String>,
    pub fetched_at: String,
    pub cache_expires_at: Option<String>,
    pub source_confidence_at_ingest: SourceConfidence,
}

impl OfferDraft {
    /// Promote a draft into a full [`ProductOffer`], stamping the now-known run
    /// id and number. The offer's own primary key is a fresh UUID — offers are
    /// append-only point-in-time snapshots, so each fetch inserts a new row.
    ///
    /// Lidl has no loyalty scheme, so the loyalty fields are always `None`.
    fn into_offer(self, run_id: &str, run_number: i64) -> ProductOffer {
        ProductOffer {
            id: uuid::Uuid::new_v4().to_string(),
            retailer_product_id: self.retailer_product_id,
            price_cents: self.price_cents,
            unit_price_eur_micros: self.unit_price_eur_micros,
            unit_price_basis: self.unit_price_basis,
            previous_price_cents: self.previous_price_cents,
            loyalty_price_cents: None,
            loyalty_scheme: None,
            offer_text: self.offer_text,
            offer_valid_until: self.offer_valid_until,
            source_snapshot_date: self.source_snapshot_date,
            fetched_at: self.fetched_at,
            cache_expires_at: self.cache_expires_at,
            last_successful_source_run_id: Some(run_id.to_string()),
            source_successful_run_number_at_fetch: Some(run_number),
            source_confidence_at_ingest: self.source_confidence_at_ingest,
        }
    }
}

/// Output of the pure normalisation step.
#[derive(Debug, Default)]
pub struct NormaliseResult {
    /// Products to upsert (one per accepted raw record). Index-aligned with
    /// [`offers`](Self::offers).
    pub products: Vec<RetailerProduct>,
    /// Offers to insert, still missing run id / number.
    pub offers: Vec<OfferDraft>,
    /// Number of raw records rejected (missing `external_id`).
    pub skipped: usize,
}

/// Summary of a completed ingest run, returned to the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub run_id: String,
    pub run_number: i64,
    pub items_fetched: i64,
    pub items_normalised: i64,
    pub skipped: usize,
    pub requests_made: i64,
}

/// What a fetch produced, plus how many live requests it cost (0 in no-network
/// mode). The request count feeds `source_runs.requests_made`.
pub struct FetchOutcome {
    pub products: Vec<RawLidlProduct>,
    pub requests_made: i64,
}

/// Normalise raw Lidl records into products + offer drafts.
///
/// Pure and database-free: every product gets a deterministic
/// [`stable_product_id`], `first_seen_at`/`last_seen_at` are both set to
/// `fetched_at` (the upsert preserves the original `first_seen_at` on conflict),
/// and every Lidl offer is stamped [`SourceConfidence::Medium`].
///
/// The unit price and basis are derived from the product *name* via
/// [`unit_price_for_product`]: Lidl supplies neither a basis nor a separate pack
/// size, so the basis is inferred from the name (design §9 categories) and the
/// pack size is parsed out of the name with `parse_pack_size`. The same scanned
/// pack-size text is stored on the product so it is queryable.
///
/// A record with a missing/blank `external_id` is **logged as an error and
/// skipped**, never inserted: a NULL `external_id` would slip past the partial
/// unique index and break deduplication.
pub fn normalise(raw: Vec<RawLidlProduct>, fetched_at: &str) -> NormaliseResult {
    let mut result = NormaliseResult::default();

    for record in raw {
        let external_id = match record.external_id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                tracing::error!(
                    name = %record.name,
                    "skipping Lidl product with missing external_id"
                );
                result.skipped += 1;
                continue;
            }
        };

        // Lidl embeds the pack size in the name and supplies no basis: derive
        // both from the name (design §9). `pack_size_text` records the size
        // parsed out of the name, rendered to its canonical *base-unit* text
        // ("2000ml" / "500g" / "6 pack"), so it is stored on the product and
        // queryable later. We pass `None` to
        // `unit_price_for_product` so it scans the *same* name for the size,
        // keeping the stored size and the priced size consistent.
        let pack_size_text =
            buywise_core::normalise::parse_pack_size(&record.name).map(render_pack_size);
        let (unit_price_eur_micros, unit_price_basis) =
            unit_price_for_product(record.price_cents, &record.name, None);

        let id = stable_product_id(Source::LidlDirect, Retailer::Lidl, &external_id);

        result.products.push(RetailerProduct {
            id: id.clone(),
            source: Source::LidlDirect,
            retailer: Retailer::Lidl,
            external_id: Some(external_id),
            url: record.url,
            name: record.name,
            brand: record.brand,
            category: record.category,
            pack_size_text,
            size_value: None,
            size_unit: None,
            pack_count: None,
            first_seen_at: fetched_at.to_string(),
            last_seen_at: fetched_at.to_string(),
        });

        result.offers.push(OfferDraft {
            retailer_product_id: id,
            price_cents: record.price_cents,
            unit_price_eur_micros,
            unit_price_basis: Some(unit_price_basis),
            previous_price_cents: record.previous_price_cents,
            offer_text: record.offer_text,
            offer_valid_until: record.offer_valid_until,
            source_snapshot_date: record.source_snapshot_date,
            fetched_at: fetched_at.to_string(),
            cache_expires_at: record.cache_expires_at,
            source_confidence_at_ingest: LIDL_CONFIDENCE,
        });
    }

    result
}

/// Run a full Lidl ingest against `pool`, using the supplied clock (`fetched_at`)
/// and `fetch` strategy.
///
/// This is the test seam: `fetch` is invoked **after** the `running` row is
/// committed, so a test can pass a closure that asserts the row is visible (or
/// that returns an error to exercise the failure path). Production callers use
/// [`ingest`] / [`spawn_ingest`], which supply the real clock and select the
/// live/no-network fetcher.
pub async fn run_ingest<F, Fut>(
    pool: &SqlitePool,
    fetched_at: &str,
    fetch: F,
) -> Result<RunSummary, IngestError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<FetchOutcome, IngestError>>,
{
    // Step 1: commit the `running` row (autocommit) before touching the network.
    let run_id = db::insert_running_source_run(pool, Source::LidlDirect, fetched_at).await?;

    match run_steps(pool, &run_id, fetched_at, fetch).await {
        Ok(summary) => Ok(summary),
        Err(err) => {
            // `run_steps` guarantees any write transaction is already rolled back,
            // so this short transaction will not deadlock on the write lock.
            if let Err(mark_err) =
                db::mark_source_run_failed(pool, &run_id, &err.to_string(), fetched_at).await
            {
                tracing::error!(
                    run_id = %run_id,
                    error = %mark_err,
                    "could not mark Lidl run as failed"
                );
            }
            Err(err)
        }
    }
}

/// Steps 2–4 of the flow. On any error it rolls back the write transaction (if
/// one was opened) before returning, so the caller can safely mark the run
/// failed without contending for the SQLite write lock.
async fn run_steps<F, Fut>(
    pool: &SqlitePool,
    run_id: &str,
    fetched_at: &str,
    fetch: F,
) -> Result<RunSummary, IngestError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<FetchOutcome, IngestError>>,
{
    // Step 2: fetch outside any transaction.
    let outcome = fetch().await?;
    let items_fetched = outcome.products.len() as i64;

    // Step 3: normalise in memory.
    let normalised = normalise(outcome.products, fetched_at);
    let items_normalised = normalised.products.len() as i64;

    // Step 4: success transaction on a single connection with BEGIN IMMEDIATE.
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

    match write_within_txn(
        &mut conn,
        run_id,
        fetched_at,
        &normalised,
        items_fetched,
        items_normalised,
        outcome.requests_made,
    )
    .await
    {
        Ok(run_number) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(RunSummary {
                run_id: run_id.to_string(),
                run_number,
                items_fetched,
                items_normalised,
                skipped: normalised.skipped,
                requests_made: outcome.requests_made,
            })
        }
        Err(err) => {
            // Roll back so the write lock is released before the caller marks the
            // run failed. Rollback failure is logged but the original error wins.
            if let Err(rb) = sqlx::query("ROLLBACK").execute(&mut *conn).await {
                tracing::error!(error = %rb, "ROLLBACK failed after Lidl write error");
            }
            Err(err)
        }
    }
}

/// The body of the success transaction. Allocates the run number under the
/// `BEGIN IMMEDIATE` lock, upserts products, inserts offers, and marks the run
/// `success`. Returns the allocated run number.
#[allow(clippy::too_many_arguments)]
async fn write_within_txn(
    conn: &mut sqlx::sqlite::SqliteConnection,
    run_id: &str,
    fetched_at: &str,
    normalised: &NormaliseResult,
    items_fetched: i64,
    items_normalised: i64,
    requests_made: i64,
) -> Result<i64, IngestError> {
    let previous = db::max_successful_run_number(conn, Source::LidlDirect).await?;
    let run_number = previous.unwrap_or(0) + 1;

    for product in &normalised.products {
        db::upsert_retailer_product(conn, product).await?;
    }
    for draft in &normalised.offers {
        let offer = draft.clone().into_offer(run_id, run_number);
        db::insert_product_offer(conn, &offer).await?;
    }

    db::finalise_successful_run(
        conn,
        run_id,
        run_number,
        fetched_at,
        requests_made,
        items_fetched,
        items_normalised,
    )
    .await?;

    Ok(run_number)
}

/// Run a Lidl ingest with the real clock, selecting the fetch strategy from the
/// environment.
///
/// * `BUYWISE_NO_NETWORK=1` → read `fixtures/lidl_offers.json`, no live calls,
///   but still write `source_runs` + `product_offers`.
/// * otherwise → live Lidl.ie fetch.
///
/// Prefer [`spawn_ingest`] at the orchestration layer so a Lidl failure cannot
/// take down the BasketWatch run; call `ingest` directly only when you want the
/// error surfaced (e.g. tests, or a Lidl-only command).
pub async fn ingest(pool: &SqlitePool) -> Result<RunSummary, IngestError> {
    let fetched_at = db::now_iso();
    if no_network() {
        tracing::info!("BUYWISE_NO_NETWORK=1: reading Lidl data from fixtures/lidl_offers.json");
        run_ingest(pool, &fetched_at, || async {
            Ok(FetchOutcome {
                products: load_fixture_offers()?,
                requests_made: 0,
            })
        })
        .await
    } else {
        run_ingest(pool, &fetched_at, live_fetch).await
    }
}

/// Orchestration entry point: run the Lidl ingest on a **detached task** so a
/// failure — *or panic* — is logged and swallowed rather than propagated.
///
/// This is the isolation boundary the issue mandates: the BasketWatch pipeline
/// must be unaffected by Lidl. The caller gets a [`JoinHandle`] it may await for
/// observability, but the returned handle **always resolves to `Ok(())`**: it
/// never yields an `IngestError` and never surfaces a `JoinError`.
///
/// To guarantee that, the real ingest runs on an *inner* `tokio::spawn`, and the
/// outer task awaits that inner handle and consumes **both** outcomes — an
/// `Err(IngestError)` (a handled run failure) and an `Err(JoinError)` (a panic
/// caught by the inner task) — logging each and then returning `()`. Awaiting the
/// outer handle therefore can never observe the panic, so a Lidl panic cannot
/// abort or fail anyone who awaits it.
pub fn spawn_ingest(pool: SqlitePool) -> JoinHandle<()> {
    spawn_swallowing(async move { ingest(&pool).await })
}

/// Run `work` on a detached task whose returned [`JoinHandle`] **always resolves
/// to `Ok(())`**, swallowing both a handled `Err(IngestError)` and a panic.
///
/// The work runs on an *inner* `tokio::spawn` so a panic inside it is captured as
/// a `JoinError` here rather than unwinding through the outer task and surfacing
/// on the handle the caller awaits. The outer task consumes all three arms — `Ok`,
/// handled `Err`, and panic — logging each and returning `()`.
fn spawn_swallowing<W>(work: W) -> JoinHandle<()>
where
    W: std::future::Future<Output = Result<RunSummary, IngestError>> + Send + 'static,
{
    tokio::spawn(async move {
        // Inner task so a panic inside `work` is converted into a `JoinError` we
        // can catch here, instead of unwinding through the outer task and
        // surfacing on the handle the caller awaits.
        let inner = tokio::spawn(work);
        match inner.await {
            Ok(Ok(summary)) => tracing::info!(
                run_id = %summary.run_id,
                run_number = summary.run_number,
                items_normalised = summary.items_normalised,
                skipped = summary.skipped,
                "Lidl ingest completed"
            ),
            Ok(Err(err)) => tracing::error!(
                error = %err,
                "Lidl ingest failed; swallowed so BasketWatch is unaffected"
            ),
            Err(join_err) => tracing::error!(
                error = %join_err,
                "Lidl ingest panicked; swallowed so BasketWatch is unaffected"
            ),
        }
    })
}

/// Render a parsed [`NormalisedSize`](buywise_core::normalise::NormalisedSize)
/// back to canonical *base-unit* pack-size text (`"2000ml"` / `"500g"` /
/// `"6 pack"`), matching the free-text format the other adapters store in
/// `pack_size_text`. Note this keeps the base unit (millilitres / grams) rather
/// than re-deriving litres/kilograms — `"2L"` is stored as `"2000ml"`.
///
/// Whole numbers print without a trailing `.0`; fractional values keep up to
/// three significant decimals (trailing zeros trimmed) so `1500.0` → `"1.5kg"`
/// is avoided in favour of the gram form `"1500g"` — we deliberately keep the
/// base unit (grams / millilitres) rather than re-deriving kg/L, which would
/// require choosing a threshold and risk mismatching the parsed size.
fn render_pack_size(size: buywise_core::normalise::NormalisedSize) -> String {
    use buywise_core::normalise::NormalisedSize;
    fn trim_number(value: f64) -> String {
        if value.fract() == 0.0 {
            format!("{}", value as i64)
        } else {
            // Up to 3 decimals, trailing zeros stripped.
            let s = format!("{value:.3}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
    }
    match size {
        NormalisedSize::Weight { grams } => format!("{}g", trim_number(grams)),
        NormalisedSize::Volume { ml } => format!("{}ml", trim_number(ml)),
        NormalisedSize::Count { each } => format!("{each} pack"),
    }
}

/// True when `BUYWISE_NO_NETWORK` is set to `1`.
fn no_network() -> bool {
    matches!(std::env::var("BUYWISE_NO_NETWORK"), Ok(v) if v == "1")
}

/// Read and parse the no-network fixture (`fixtures/lidl_offers.json`).
///
/// The path is resolved relative to the crate's `CARGO_MANIFEST_DIR` so it works
/// regardless of the process CWD.
fn load_fixture_offers() -> Result<Vec<RawLidlProduct>, IngestError> {
    let path = format!(
        "{}/../../fixtures/lidl_offers.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).map_err(|source| IngestError::Fixture {
        path: path.clone(),
        source,
    })?;
    let products = serde_json::from_slice(&bytes)?;
    Ok(products)
}

/// Live Lidl.ie fetch.
///
/// NOTE: Lidl.ie's exact endpoint/response shape — and indeed whether a stable
/// direct feed exists at all — is an open question (design §16). The control flow
/// here (build a timed client, fetch, deserialise into [`RawLidlProduct`]) is
/// final and mirrors the fixture shape; the host/path and JSON field mapping may
/// need adjustment once the real feed is confirmed, so the base URL is read from
/// the environment (`LIDL_FEED_URL`) and can be corrected without a code change.
async fn live_fetch() -> Result<FetchOutcome, IngestError> {
    let url = std::env::var("LIDL_FEED_URL")
        .unwrap_or_else(|_| "https://www.lidl.ie/q/api/search".into());

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;

    let response = client.get(&url).send().await?.error_for_status()?;
    // A 2xx with a non-product body is a *shape* failure, surfaced as
    // IngestError::Parse so the run is marked failed rather than panicking.
    let products: Vec<RawLidlProduct> = response.json().await.map_err(|err| {
        IngestError::Parse(format!("Lidl.ie response was not a product listing: {err}"))
    })?;

    Ok(FetchOutcome {
        products,
        requests_made: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use buywise_core::UnitPriceBasis;

    fn raw(external_id: Option<&str>, name: &str, price_cents: Cents) -> RawLidlProduct {
        RawLidlProduct {
            external_id: external_id.map(String::from),
            name: name.to_string(),
            brand: None,
            category: None,
            url: None,
            price_cents,
            previous_price_cents: None,
            offer_text: None,
            offer_valid_until: None,
            source_snapshot_date: None,
            cache_expires_at: None,
        }
    }

    #[test]
    fn normalise_builds_products_and_offers_with_medium_confidence() {
        let result = normalise(
            vec![raw(Some("lidl-1"), "Lidl Semi Skimmed Milk 2L", 199)],
            "2026-05-30T00:00:00.000Z",
        );
        assert_eq!(result.products.len(), 1);
        assert_eq!(result.offers.len(), 1);
        assert_eq!(result.skipped, 0);

        let product = &result.products[0];
        assert_eq!(product.source, Source::LidlDirect);
        assert_eq!(product.retailer, Retailer::Lidl);
        assert_eq!(product.external_id.as_deref(), Some("lidl-1"));
        // Deterministic id, keyed on the Lidl source + retailer.
        assert_eq!(
            product.id,
            stable_product_id(Source::LidlDirect, Retailer::Lidl, "lidl-1")
        );
        assert_eq!(product.first_seen_at, "2026-05-30T00:00:00.000Z");
        assert_eq!(product.last_seen_at, "2026-05-30T00:00:00.000Z");

        // Confidence is medium for every Lidl offer (acceptance criterion).
        let offer = &result.offers[0];
        assert_eq!(offer.retailer_product_id, product.id);
        assert_eq!(offer.source_confidence_at_ingest, SourceConfidence::Medium);
    }

    #[test]
    fn normalise_extracts_pack_size_and_unit_price_from_name() {
        // "2L" is embedded in the name; parse_pack_size pulls it out and
        // unit_price_for_product computes €1.99 / 2L → €0.995/L.
        let result = normalise(
            vec![raw(Some("lidl-1"), "Lidl Semi Skimmed Milk 2L", 199)],
            "2026-05-30T00:00:00.000Z",
        );
        let product = &result.products[0];
        // "2L" renders to its base-unit canonical form, millilitres.
        assert_eq!(product.pack_size_text.as_deref(), Some("2000ml"));

        let offer = &result.offers[0];
        assert_eq!(offer.unit_price_basis, Some(UnitPriceBasis::PerL));
        assert_eq!(offer.unit_price_eur_micros, Some(995_000));
    }

    #[test]
    fn normalise_keeps_offer_when_name_has_no_size() {
        // No parseable size: pack_size_text is NULL and there is no unit price,
        // but the row is still kept with its inferred basis.
        let result = normalise(
            vec![raw(Some("lidl-2"), "Lidl Mystery Item", 250)],
            "2026-05-30T00:00:00.000Z",
        );
        assert_eq!(result.products.len(), 1);
        assert!(result.products[0].pack_size_text.is_none());
        assert_eq!(result.offers[0].unit_price_eur_micros, None);
        // The inferred basis is still recorded.
        assert!(result.offers[0].unit_price_basis.is_some());
    }

    #[test]
    fn normalise_skips_missing_external_id() {
        let result = normalise(
            vec![
                raw(Some("lidl-1"), "Lidl Milk 2L", 199),
                raw(None, "No ID 1L", 120),
                raw(Some("   "), "Blank ID 500g", 90),
            ],
            "2026-05-30T00:00:00.000Z",
        );
        assert_eq!(result.products.len(), 1, "only the valid record survives");
        assert_eq!(result.offers.len(), 1);
        assert_eq!(result.skipped, 2, "null and blank external_id both skipped");
    }

    #[test]
    fn normalise_preserves_brand_and_url() {
        let mut r = raw(Some("lidl-9"), "Lidl Greek Style Yogurt 500g", 150);
        r.brand = Some("Milbona".into());
        r.url = Some("https://www.lidl.ie/p/lidl-9".into());
        let result = normalise(vec![r], "2026-05-30T00:00:00.000Z");
        assert_eq!(result.products[0].brand.as_deref(), Some("Milbona"));
        assert_eq!(
            result.products[0].url.as_deref(),
            Some("https://www.lidl.ie/p/lidl-9")
        );
    }

    /// The isolation contract for a *panic* (not just an `Err`): a panic inside
    /// the swallowed work must be caught by the inner task so the handle the
    /// caller awaits still resolves to `Ok(())`. Without the inner `tokio::spawn`
    /// the panic would surface as a `JoinError` and `handle.await` would be `Err`.
    #[tokio::test]
    async fn spawn_swallowing_swallows_a_panic() {
        let handle = spawn_swallowing(async {
            panic!("boom: simulated ingest panic");
        });
        // The outer handle resolves Ok despite the inner panic — the panic was
        // contained and logged, never propagated to the awaiting caller.
        handle
            .await
            .expect("panic must be swallowed: handle resolves to Ok(())");
    }

    #[test]
    fn into_offer_stamps_run_metadata_and_nulls_loyalty() {
        let result = normalise(
            vec![raw(Some("lidl-1"), "Lidl Milk 2L", 199)],
            "2026-05-30T00:00:00.000Z",
        );
        let offer = result.offers[0].clone().into_offer("run-7", 42);
        assert_eq!(
            offer.last_successful_source_run_id.as_deref(),
            Some("run-7")
        );
        assert_eq!(offer.source_successful_run_number_at_fetch, Some(42));
        assert!(!offer.id.is_empty(), "offer gets a fresh id");
        // Lidl has no loyalty programme.
        assert_eq!(offer.loyalty_price_cents, None);
        assert_eq!(offer.loyalty_scheme, None);
    }
}
