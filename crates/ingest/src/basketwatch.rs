//! BasketWatch RapidAPI ingest adapter.
//!
//! Fetches price data for Tesco, Dunnes Stores, SuperValu and Aldi from the
//! BasketWatch RapidAPI and normalises it into `retailer_products` +
//! `product_offers` rows. (Lidl is handled separately by the P0-T9 adapter.)
//!
//! # Run flow (see `docs/implementation-plan.md` P0-T8 and design §7/§12)
//!
//! 1. Commit a `source_runs` row with `status = 'running'` — autocommit, *before*
//!    any network I/O, so a crash mid-fetch still leaves an auditable record.
//! 2. Fetch everything from the API (or, under `BUYWISE_NO_NETWORK=1`, from
//!    `fixtures/offers.json`). No write transaction is open during this step.
//! 3. Normalise the raw payload into in-memory [`RetailerProduct`] + [`OfferDraft`]
//!    values; the raw JSON is dropped immediately.
//! 4. On success, on a single connection: `BEGIN IMMEDIATE` → allocate
//!    `MAX(successful_run_number) + 1` → upsert products → insert offers (stamped
//!    with the run id + number) → mark the run `success` → `COMMIT`.
//! 5. On *any* failure: if a write transaction was open it is rolled back first
//!    (releasing the SQLite write lock), then the original `running` row is marked
//!    `failed` in a separate short transaction.
//!
//! The normaliser is deliberately a **pure function** ([`normalise`]) that knows
//! nothing about the database or the run number: the run number is only valid
//! once allocated inside the step-4 transaction, so offers are carried as
//! [`OfferDraft`]s and stamped at insert time.

use std::time::Duration;

use buywise_core::db;
use buywise_core::normalise::validate_unit_price;
use buywise_core::{
    Cents, Micros, ProductOffer, Retailer, RetailerProduct, Source, SourceConfidence,
    UnitPriceBasis,
};
use serde::Deserialize;
use sqlx::sqlite::SqlitePool;

use crate::error::IngestError;
use crate::product_id::stable_product_id;

/// Monthly request thresholds (design §12). Warn, then skip non-essential
/// refreshes, then hard-stop all external calls.
const QUOTA_WARN: i64 = 3_500;
const QUOTA_SKIP_NON_ESSENTIAL: i64 = 4_500;
const QUOTA_HARD_STOP: i64 = 4_900;

/// Self-imposed 40 RPM cap (buffer below BasketWatch's 60 RPM Pro limit): one
/// request per 1.5 s.
const RATE_LIMIT_INTERVAL: Duration = Duration::from_millis(1_500);

/// Retailers BasketWatch covers (Lidl is the separate P0-T9 adapter).
const RETAILERS: [Retailer; 4] = [
    Retailer::Tesco,
    Retailer::Dunnes,
    Retailer::SuperValu,
    Retailer::Aldi,
];

/// One product+offer record as delivered by BasketWatch (and as stored in
/// `fixtures/offers.json`).
///
/// `external_id` is `Option` precisely so a missing id can be detected and the
/// row skipped rather than silently inserted — a NULL `external_id` would bypass
/// the partial unique index and defeat deduplication. `brand` is carried as a
/// top-level field here; if the live API does not surface brand at the top level
/// it must be parsed out of `name` so it is not silently NULL (the optimiser's
/// brand constraints depend on it).
#[derive(Debug, Clone, Deserialize)]
pub struct RawBasketWatchProduct {
    /// Retailer-assigned product id. Missing/blank ⇒ the row is skipped.
    pub external_id: Option<String>,
    /// Retailer slug (`tesco` | `dunnes` | `supervalu` | `aldi`).
    pub retailer: String,
    pub name: String,
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub pack_size_text: Option<String>,
    #[serde(default)]
    pub size_value: Option<f64>,
    #[serde(default)]
    pub size_unit: Option<String>,
    #[serde(default)]
    pub pack_count: Option<i32>,
    pub price_cents: Cents,
    #[serde(default)]
    pub unit_price_eur_micros: Option<Micros>,
    /// Raw unit-price basis string; parsed leniently into [`UnitPriceBasis`].
    #[serde(default)]
    pub unit_price_basis: Option<String>,
    #[serde(default)]
    pub previous_price_cents: Option<Cents>,
    #[serde(default)]
    pub loyalty_price_cents: Option<Cents>,
    #[serde(default)]
    pub loyalty_scheme: Option<String>,
    #[serde(default)]
    pub offer_text: Option<String>,
    #[serde(default)]
    pub offer_valid_until: Option<String>,
    #[serde(default)]
    pub source_snapshot_date: Option<String>,
    #[serde(default)]
    pub cache_expires_at: Option<String>,
    /// Explicit physical form (Backlog-T1), one of
    /// [`buywise_core::domain::FORM_FACTORS`]. When present it takes precedence
    /// over the name/category keyword scan; absent ⇒ best-effort derivation.
    #[serde(default)]
    pub form_factor: Option<String>,
    /// Explicit dietary flags (Backlog-T1), a subset of
    /// [`buywise_core::domain::DIETARY_FLAGS`]. When present it takes precedence
    /// over the keyword scan; absent ⇒ best-effort derivation.
    #[serde(default)]
    pub dietary_flags: Option<Vec<String>>,
}

/// An offer pending insertion: every `product_offers` field *except* the ones
/// only known inside the success transaction (the offer's own id, the run id,
/// and the run number). Carrying these as drafts keeps [`normalise`] pure and
/// prevents stamping an unallocated run number.
#[derive(Debug, Clone, PartialEq)]
pub struct OfferDraft {
    pub retailer_product_id: String,
    pub price_cents: Cents,
    pub unit_price_eur_micros: Option<Micros>,
    pub unit_price_basis: Option<UnitPriceBasis>,
    pub previous_price_cents: Option<Cents>,
    pub loyalty_price_cents: Option<Cents>,
    pub loyalty_scheme: Option<String>,
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
    /// `pub` so out-of-crate callers that seed fixtures from the same draft type
    /// (the CLI's fixture-mode database builder) can stamp drafts without
    /// re-implementing the field mapping.
    pub fn into_offer(self, run_id: &str, run_number: i64) -> ProductOffer {
        ProductOffer {
            id: uuid::Uuid::new_v4().to_string(),
            retailer_product_id: self.retailer_product_id,
            price_cents: self.price_cents,
            unit_price_eur_micros: self.unit_price_eur_micros,
            unit_price_basis: self.unit_price_basis,
            previous_price_cents: self.previous_price_cents,
            loyalty_price_cents: self.loyalty_price_cents,
            loyalty_scheme: self.loyalty_scheme,
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
    /// Number of raw records rejected (missing `external_id` or bad retailer).
    pub skipped: usize,
}

/// Summary of a completed ingest run, returned to the caller (e.g. the CLI).
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
    pub products: Vec<RawBasketWatchProduct>,
    pub requests_made: i64,
}

/// Normalise raw BasketWatch records into products + offer drafts.
///
/// Pure and database-free: every product gets a deterministic
/// [`stable_product_id`], `first_seen_at`/`last_seen_at` are both set to
/// `fetched_at` (the upsert preserves the original `first_seen_at` on conflict),
/// and BasketWatch offers are stamped `SourceConfidence::High`.
///
/// A record with a missing/blank `external_id` — or an unrecognised `retailer` —
/// is **logged as an error and skipped**, never inserted: a NULL `external_id`
/// would slip past the partial unique index and break deduplication.
pub fn normalise(raw: Vec<RawBasketWatchProduct>, fetched_at: &str) -> NormaliseResult {
    let mut result = NormaliseResult::default();

    for record in raw {
        let external_id = match record.external_id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => {
                tracing::error!(
                    name = %record.name,
                    "skipping BasketWatch product with missing external_id"
                );
                result.skipped += 1;
                continue;
            }
        };

        let retailer = match Retailer::try_from(record.retailer.as_str()) {
            Ok(r) => r,
            Err(_) => {
                tracing::error!(
                    name = %record.name,
                    retailer = %record.retailer,
                    "skipping BasketWatch product with unrecognised retailer"
                );
                result.skipped += 1;
                continue;
            }
        };

        // Lenient unit-price basis: an unknown string is dropped (logged) rather
        // than failing the whole run.
        let provided_basis = match record.unit_price_basis.as_deref() {
            None => None,
            Some(s) => match UnitPriceBasis::try_from(s) {
                Ok(b) => Some(b),
                Err(_) => {
                    tracing::warn!(basis = %s, name = %record.name, "unknown unit_price_basis; storing NULL");
                    None
                }
            },
        };

        // Cross-check BasketWatch's unit price against one computed from the pack
        // size (P0-T7): if the provided value is missing, or diverges by more than
        // 2%, `validate_unit_price` substitutes the recomputed value — so a bad or
        // absent API unit price cannot flow straight into `product_offers`.
        let (unit_price_eur_micros, unit_price_basis) = validate_unit_price(
            record.price_cents,
            record.pack_size_text.as_deref(),
            record.unit_price_eur_micros,
            provided_basis,
        );

        let id = stable_product_id(Source::BasketWatch, retailer, &external_id);

        // Backlog-T1: prefer explicit feed form/dietary fields; otherwise derive
        // them best-effort from name + category rather than writing NULL.
        let (form_factor, dietary_flags) = crate::attributes::resolve_attributes(
            &record.name,
            record.category.as_deref(),
            record.form_factor,
            record.dietary_flags,
        );

        result.products.push(RetailerProduct {
            id: id.clone(),
            source: Source::BasketWatch,
            retailer,
            external_id: Some(external_id),
            url: record.url,
            name: record.name,
            brand: record.brand,
            category: record.category,
            pack_size_text: record.pack_size_text,
            size_value: record.size_value,
            size_unit: record.size_unit,
            pack_count: record.pack_count,
            form_factor,
            dietary_flags,
            first_seen_at: fetched_at.to_string(),
            last_seen_at: fetched_at.to_string(),
        });

        result.offers.push(OfferDraft {
            retailer_product_id: id,
            price_cents: record.price_cents,
            unit_price_eur_micros,
            unit_price_basis,
            previous_price_cents: record.previous_price_cents,
            loyalty_price_cents: record.loyalty_price_cents,
            loyalty_scheme: record.loyalty_scheme,
            offer_text: record.offer_text,
            offer_valid_until: record.offer_valid_until,
            source_snapshot_date: record.source_snapshot_date,
            fetched_at: fetched_at.to_string(),
            cache_expires_at: record.cache_expires_at,
            source_confidence_at_ingest: SourceConfidence::High,
        });
    }

    result
}

/// Run a full BasketWatch ingest against `pool`, using the supplied clock
/// (`fetched_at`) and `fetch` strategy.
///
/// This is the test seam: `fetch` is invoked **after** the `running` row is
/// committed, so a test can pass a closure that asserts the row is visible
/// before returning fixture data. Production callers use [`ingest`], which
/// supplies the real clock and selects the live/no-network fetcher.
///
/// `fetched_at` is injected (rather than read from the wall clock inside) so
/// timestamps are fully deterministic in tests — two consecutive runs can be
/// given distinct timestamps to prove `last_seen_at` advances on re-ingest.
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
    let run_id = db::insert_running_source_run(pool, Source::BasketWatch, fetched_at).await?;

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
                    "could not mark BasketWatch run as failed"
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
                tracing::error!(error = %rb, "ROLLBACK failed after ingest write error");
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
    let previous = db::max_successful_run_number(conn, Source::BasketWatch).await?;
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

/// Run a BasketWatch ingest with the real clock, selecting the fetch strategy
/// from the environment.
///
/// * `BUYWISE_NO_NETWORK=1` → read `fixtures/offers.json`, no live calls, but
///   still write `source_runs` + `product_offers`.
/// * otherwise → live RapidAPI fetch (requires `BASKETWATCH_API_KEY`).
pub async fn ingest(pool: &SqlitePool) -> Result<RunSummary, IngestError> {
    let fetched_at = db::now_iso();
    if no_network() {
        tracing::info!("BUYWISE_NO_NETWORK=1: reading BasketWatch data from fixtures/offers.json");
        run_ingest(pool, &fetched_at, || async {
            Ok(FetchOutcome {
                products: load_fixture_offers()?,
                requests_made: 0,
            })
        })
        .await
    } else {
        // One clone for the closure; the borrow passed to `run_ingest` reuses the
        // caller's reference.
        let fetch_pool = pool.clone();
        let clock = fetched_at.clone();
        run_ingest(pool, &fetched_at, move || live_fetch(fetch_pool, clock)).await
    }
}

/// True when `BUYWISE_NO_NETWORK` is set to `1`.
fn no_network() -> bool {
    matches!(std::env::var("BUYWISE_NO_NETWORK"), Ok(v) if v == "1")
}

/// `YYYY-MM` prefix of an ISO timestamp, for monthly quota bucketing.
fn month_of(timestamp: &str) -> String {
    timestamp.get(..7).unwrap_or(timestamp).to_string()
}

/// Read and parse the no-network fixture (`fixtures/offers.json`).
///
/// The path is resolved relative to the crate's `CARGO_MANIFEST_DIR` so it works
/// regardless of the process CWD (dry-run is a dev/CI affordance, and this is the
/// only place the file is loaded by the adapter).
fn load_fixture_offers() -> Result<Vec<RawBasketWatchProduct>, IngestError> {
    let path = format!("{}/../../fixtures/offers.json", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).map_err(|source| IngestError::Fixture {
        path: path.clone(),
        source,
    })?;
    let products = serde_json::from_slice(&bytes)?;
    Ok(products)
}

/// Live RapidAPI fetch across all four retailers, enforcing the monthly quota
/// guardrails and the 40 RPM self-cap.
///
/// NOTE: BasketWatch's exact endpoint/response shape is an open question
/// (design §16). The control flow here — key check, guardrails, throttle, usage
/// accounting — is final; the per-retailer URL and the JSON field mapping
/// (deserialised straight into [`RawBasketWatchProduct`]) mirror the fixture
/// shape and may need adjustment once the real API is confirmed. The host and
/// path template are read from the environment so they can be corrected without
/// a code change.
async fn live_fetch(pool: SqlitePool, fetched_at: String) -> Result<FetchOutcome, IngestError> {
    // An empty or whitespace-only key is treated as missing, not present:
    // systemd's EnvironmentFile exports a bare `BASKETWATCH_API_KEY=` line as
    // `Ok("")`, which would otherwise slip past into a live request and fail
    // with a confusing RapidAPI error instead of the clear `MissingApiKey`
    // (see `crate::configured_env`).
    let api_key = crate::configured_env("BASKETWATCH_API_KEY").ok_or(IngestError::MissingApiKey)?;
    let month = month_of(&fetched_at);

    // Informational classification only (the warn / skip log). Enforcement is the
    // per-request atomic reservation below — a single up-front read would be stale
    // under concurrency. The Phase-0 weekly batch is a full refresh with no
    // per-item currency, so every request is essential by construction; there is
    // nothing to drop at the skip threshold, which is instead honoured by the
    // hard stop capping how far the batch can run.
    let used = db::current_month_request_count(&pool, Source::BasketWatch, &month).await?;
    match quota_action(used) {
        QuotaAction::SkipNonEssential => tracing::warn!(
            used,
            "above skip-non-essential threshold; Phase-0 batch is essential-only"
        ),
        QuotaAction::Warn => tracing::warn!(
            used,
            limit = QUOTA_HARD_STOP,
            "BasketWatch monthly request budget high"
        ),
        QuotaAction::HardStop | QuotaAction::Proceed => {}
    }

    // An empty/whitespace-only override falls back to the default rather than
    // sending requests to an empty host: a bare `BASKETWATCH_API_HOST=` line
    // (which systemd exports as `Ok("")`, and the upgrade note may seed) means
    // "use the default", not "use no host" (see `crate::configured_env`).
    let host = crate::configured_env("BASKETWATCH_API_HOST")
        .unwrap_or_else(|| "basketwatch.p.rapidapi.com".into());
    // A per-request timeout bounds a stalled `send()`; otherwise a hung response
    // would keep the `running` row open indefinitely.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let mut products = Vec::new();
    let mut requests_made = 0i64;
    // Capture the first error rather than `?`-returning it, so partial progress is
    // returned cleanly (the reservations already committed stay recorded).
    let mut fetch_error: Option<IngestError> = None;
    for retailer in RETAILERS {
        // Atomically reserve a request slot before each call. This re-reads the
        // live month-to-date count under a write lock and records the request in
        // one step, so concurrent ingest processes cannot both pass a stale check
        // and overshoot the 4900 hard stop.
        let event_id = match db::reserve_request_slot(
            &pool,
            Source::BasketWatch,
            "products",
            &month,
            &fetched_at,
            QUOTA_HARD_STOP,
        )
        .await?
        {
            db::RequestReservation::Granted { event_id, .. } => event_id,
            db::RequestReservation::Denied { used } => {
                if requests_made == 0 {
                    // Could not even start: surface the hard stop as an error.
                    return Err(IngestError::QuotaHardStop { used });
                }
                tracing::warn!(
                    used,
                    requests_made,
                    "BasketWatch hard stop reached mid-batch; stopping external calls"
                );
                break;
            }
        };

        // 40 RPM self-cap: space requests by RATE_LIMIT_INTERVAL (skip the wait
        // before the first request).
        if requests_made > 0 {
            tokio::time::sleep(RATE_LIMIT_INTERVAL).await;
        }

        let url = format!("https://{host}/products?retailer={}", retailer.as_db_str());
        let response = match client
            .get(&url)
            .header("X-RapidAPI-Key", &api_key)
            .header("X-RapidAPI-Host", &host)
            .send()
            .await
        {
            // A response (even a 4xx/5xx) means the request reached BasketWatch
            // and genuinely consumed quota — keep the reservation.
            Ok(response) => {
                requests_made += 1;
                response
            }
            // `send()` failed. Only a clearly pre-send failure — the connection
            // was never established — proves the request did not reach RapidAPI
            // and consumed no quota; refund that slot. Any ambiguous failure
            // (notably a timeout, which can fire *after* the server received and
            // charged the call) keeps the reservation, the conservative direction
            // for a quota guardrail.
            Err(err) => {
                if err.is_connect() {
                    if let Err(refund) = db::refund_request_slot(&pool, &event_id).await {
                        tracing::error!(error = %refund, "failed to refund reserved request slot");
                    }
                } else {
                    // Keep the reservation and count it, so the run's
                    // requests_made stays consistent with the recorded usage.
                    requests_made += 1;
                    tracing::warn!(
                        error = %err,
                        "ambiguous send failure; keeping reserved quota slot"
                    );
                }
                fetch_error = Some(err.into());
                break;
            }
        };

        match response.error_for_status() {
            Ok(ok) => match ok.json::<Vec<RawBasketWatchProduct>>().await {
                Ok(batch) => products.extend(batch),
                Err(err) => {
                    fetch_error = Some(err.into());
                    break;
                }
            },
            Err(err) => {
                fetch_error = Some(err.into());
                break;
            }
        }
    }

    if let Some(err) = fetch_error {
        return Err(err);
    }

    Ok(FetchOutcome {
        products,
        requests_made,
    })
}

/// Which guardrail applies at a given month-to-date request count (design §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaAction {
    /// Below all thresholds — fetch normally.
    Proceed,
    /// At/above the warn threshold — fetch, but log a budget warning.
    Warn,
    /// At/above the skip threshold — fetch only essential (non-current) items.
    SkipNonEssential,
    /// At/above the hard stop — make no external calls.
    HardStop,
}

/// Classify a month-to-date request count against the quota thresholds. Pure, so
/// the threshold boundaries are unit-tested without touching the network.
pub fn quota_action(used: i64) -> QuotaAction {
    if used >= QUOTA_HARD_STOP {
        QuotaAction::HardStop
    } else if used >= QUOTA_SKIP_NON_ESSENTIAL {
        QuotaAction::SkipNonEssential
    } else if used >= QUOTA_WARN {
        QuotaAction::Warn
    } else {
        QuotaAction::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(external_id: Option<&str>, retailer: &str, name: &str) -> RawBasketWatchProduct {
        RawBasketWatchProduct {
            external_id: external_id.map(String::from),
            retailer: retailer.to_string(),
            name: name.to_string(),
            brand: None,
            category: None,
            url: None,
            pack_size_text: None,
            size_value: None,
            size_unit: None,
            pack_count: None,
            price_cents: 199,
            unit_price_eur_micros: None,
            unit_price_basis: None,
            previous_price_cents: None,
            loyalty_price_cents: None,
            loyalty_scheme: None,
            offer_text: None,
            offer_valid_until: None,
            source_snapshot_date: None,
            cache_expires_at: None,
            form_factor: None,
            dietary_flags: None,
        }
    }

    #[test]
    fn normalise_builds_products_and_offers() {
        let result = normalise(
            vec![raw(Some("t-1"), "tesco", "Milk 1L")],
            "2026-05-30T00:00:00.000Z",
        );
        assert_eq!(result.products.len(), 1);
        assert_eq!(result.offers.len(), 1);
        assert_eq!(result.skipped, 0);

        let product = &result.products[0];
        assert_eq!(product.source, Source::BasketWatch);
        assert_eq!(product.retailer, Retailer::Tesco);
        assert_eq!(product.external_id.as_deref(), Some("t-1"));
        // Deterministic id.
        assert_eq!(
            product.id,
            stable_product_id(Source::BasketWatch, Retailer::Tesco, "t-1")
        );
        // first_seen_at == last_seen_at == fetched_at on a fresh record.
        assert_eq!(product.first_seen_at, "2026-05-30T00:00:00.000Z");
        assert_eq!(product.last_seen_at, "2026-05-30T00:00:00.000Z");

        // Drafts carry no run number yet; confidence is high for BasketWatch.
        let offer = &result.offers[0];
        assert_eq!(offer.retailer_product_id, product.id);
        assert_eq!(offer.source_confidence_at_ingest, SourceConfidence::High);
    }

    #[test]
    fn normalise_skips_missing_external_id() {
        let result = normalise(
            vec![
                raw(Some("t-1"), "tesco", "Milk"),
                raw(None, "tesco", "No ID"),
                raw(Some("   "), "tesco", "Blank ID"),
            ],
            "2026-05-30T00:00:00.000Z",
        );
        assert_eq!(result.products.len(), 1, "only the valid record survives");
        assert_eq!(result.offers.len(), 1);
        assert_eq!(result.skipped, 2, "null and blank external_id both skipped");
    }

    #[test]
    fn normalise_skips_unknown_retailer() {
        let result = normalise(
            vec![raw(Some("x-1"), "ubereats", "Milk")],
            "2026-05-30T00:00:00.000Z",
        );
        assert!(result.products.is_empty());
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn normalise_preserves_brand() {
        let mut r = raw(Some("t-9"), "tesco", "Fairy Washing Up Liquid 450ml");
        r.brand = Some("Fairy".into());
        let result = normalise(vec![r], "2026-05-30T00:00:00.000Z");
        assert_eq!(result.products[0].brand.as_deref(), Some("Fairy"));
    }

    #[test]
    fn normalise_validates_and_recomputes_unit_price() {
        // €2.50 / 433 ml → €5.77/L. A provided value that is correct is trusted…
        let mut good = raw(Some("t-1"), "tesco", "Fairy Washing Up Liquid 433ml");
        good.pack_size_text = Some("433ml".into());
        good.price_cents = 250;
        good.unit_price_basis = Some("per_l".into());
        good.unit_price_eur_micros = Some(5_773_672);

        // …while a value off by 10× (well beyond the 2% tolerance) is recomputed.
        let mut bad = raw(Some("t-2"), "tesco", "Fairy Washing Up Liquid 433ml");
        bad.pack_size_text = Some("433ml".into());
        bad.price_cents = 250;
        bad.unit_price_basis = Some("per_l".into());
        bad.unit_price_eur_micros = Some(577_367);

        // …and a missing value is supplemented from the pack size.
        let mut missing = raw(Some("t-3"), "tesco", "Fairy Washing Up Liquid 433ml");
        missing.pack_size_text = Some("433ml".into());
        missing.price_cents = 250;
        missing.unit_price_basis = Some("per_l".into());
        missing.unit_price_eur_micros = None;

        let result = normalise(vec![good, bad, missing], "2026-05-30T00:00:00.000Z");
        assert_eq!(result.offers[0].unit_price_eur_micros, Some(5_773_672));
        assert_eq!(result.offers[1].unit_price_eur_micros, Some(5_773_672));
        assert_eq!(result.offers[2].unit_price_eur_micros, Some(5_773_672));
    }

    #[test]
    fn normalise_parses_unit_price_basis_leniently() {
        let mut good = raw(Some("a"), "aldi", "Milk");
        good.unit_price_basis = Some("per_l".into());
        let mut bad = raw(Some("b"), "aldi", "Cheese");
        bad.unit_price_basis = Some("per_furlong".into());

        let result = normalise(vec![good, bad], "2026-05-30T00:00:00.000Z");
        assert_eq!(result.offers.len(), 2, "bad basis does not drop the row");
        assert_eq!(
            result.offers[0].unit_price_basis,
            Some(UnitPriceBasis::PerL)
        );
        assert_eq!(result.offers[1].unit_price_basis, None);
    }

    #[test]
    fn into_offer_stamps_run_metadata() {
        let result = normalise(
            vec![raw(Some("t-1"), "tesco", "Milk")],
            "2026-05-30T00:00:00.000Z",
        );
        let offer = result.offers[0].clone().into_offer("run-7", 42);
        assert_eq!(
            offer.last_successful_source_run_id.as_deref(),
            Some("run-7")
        );
        assert_eq!(offer.source_successful_run_number_at_fetch, Some(42));
        assert!(!offer.id.is_empty(), "offer gets a fresh id");
    }

    #[test]
    fn month_of_extracts_year_month() {
        assert_eq!(month_of("2026-05-30T12:00:00.000Z"), "2026-05");
        assert_eq!(month_of("short"), "short");
    }

    #[test]
    fn quota_action_respects_threshold_boundaries() {
        assert_eq!(quota_action(0), QuotaAction::Proceed);
        assert_eq!(quota_action(QUOTA_WARN - 1), QuotaAction::Proceed);
        assert_eq!(quota_action(QUOTA_WARN), QuotaAction::Warn);
        assert_eq!(
            quota_action(QUOTA_SKIP_NON_ESSENTIAL - 1),
            QuotaAction::Warn
        );
        assert_eq!(
            quota_action(QUOTA_SKIP_NON_ESSENTIAL),
            QuotaAction::SkipNonEssential
        );
        assert_eq!(
            quota_action(QUOTA_HARD_STOP - 1),
            QuotaAction::SkipNonEssential
        );
        assert_eq!(quota_action(QUOTA_HARD_STOP), QuotaAction::HardStop);
        assert_eq!(quota_action(10_000), QuotaAction::HardStop);
    }
}
