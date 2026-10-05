//! Basket-optimisation route (`POST /api/optimise`) — P4-T1.
//!
//! Runs the core optimiser (`buywise_core::optimiser`) over a stored shopping
//! list and returns the four basket plans plus a deterministic plain-language
//! explanation, and persists a `recommendation_runs` row for later correlation
//! with savings events.
//!
//! # Pipeline
//!
//! 1. Authorize: the caller must be a member of the list's household
//!    (`AuthenticatedUser` + the same `household_members` check `lists.rs` uses).
//!    CSRF is enforced by the extractor (this is a POST).
//! 2. Build [`OptimiserInput`] from the list's ACTIVE entries. Unlike the CLI
//!    (which resolves raw fixture text), the list entries are *already* resolved:
//!    each carries `resolution_status` and `canonical_item_id`. An entry counts
//!    as priceable only when `resolution_status = 'resolved'` (anything else is
//!    surfaced as unresolved). For each resolved entry we load its
//!    `default_constraint_policy` and `item_constraints` row exactly as the CLI's
//!    `build_optimiser_input` does. The one deliberate difference: the CLI also
//!    runs `merge_requested_size` to fold a per-line size parsed from raw text
//!    (e.g. `"milk 2L"`) into the constraints. The API has no such input —
//!    `list_entries` persists no per-line requested size, only the resolved
//!    `canonical_item_id` and `quantity` — so that step is a no-op here and the
//!    resulting baskets match the CLI for already-resolved entries.
//! 3. Run [`optimise`] against the pool.
//! 4. Derive `offer_snapshot_date` (latest `source_snapshot_date` among the
//!    offers backing the recommended plan's lines) and a deterministic
//!    `plain_language_explanation` (per-category cheaper-for phrasing).
//! 5. Persist one `recommendation_runs` row and return its id as `run_id`.
//!
//! # Thresholds
//!
//! `convenience_threshold_2`/`convenience_threshold_3` are cents and OPTIONAL in
//! the request. An absent value falls back to the household's stored
//! `household_settings` row (P4-T2, see [`crate::settings`]); only if that
//! household has no settings row do the documented core defaults apply
//! ([`buywise_core::optimiser::DEFAULT_THRESHOLDS`] — €10 / €25). Phase-0/Phase-4 plans
//! are single- and two-store only, so threshold-3 has no effect on the plans
//! today; it is accepted and persisted for forward-compatibility (and validated:
//! a third store can never be easier to justify than a second).
//!
//! # Store settings
//!
//! The household's per-retailer settings (issue #98) are read alongside the
//! thresholds and passed to the core: `active` stores always take part,
//! `occasional` stores only when the request's `trip_retailers` marks a trip to
//! them today, and `off` stores never do. The request's `trip_retailers` has no
//! effect on `active`/`off` stores.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::db::{self, DbError};
use buywise_core::domain::{Cents, ConstraintPolicy, Retailer};
use buywise_core::optimiser::{
    optimise, BasketItem, BasketItemIdentity, NeedsConfirmationItem, NoValidOfferItem,
    OptimiserInput, OptimiserOutput, PlanKind, PlanReason, ReasonCode, Recommendation,
    RecommendedKind, StorePlan, StoreSetting, UnresolvedItem, DEFAULT_THRESHOLDS,
};

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the optimise route as a `Router<AppState>`, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/api/optimise", post(optimise_list))
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// Body for `POST /api/optimise`.
///
/// `list_id` is required. Both thresholds are optional cents; an absent value
/// uses the household default (see the module docs).
#[derive(Debug, Deserialize)]
pub struct OptimiseRequest {
    /// The list to optimise. Must exist and belong to a household the caller is a
    /// member of.
    pub list_id: String,
    /// Second-store convenience threshold in cents; optional (household default).
    pub convenience_threshold_2: Option<Cents>,
    /// Third-store convenience threshold in cents; optional (household default).
    pub convenience_threshold_3: Option<Cents>,
    /// Retailers the household is visiting today (issue #98). An `occasional`
    /// store listed here takes part in this run's plans; a store left out is not
    /// planned for even if it is cheap. Optional (defaults to none) and has no
    /// effect on `active`/`off` stores. Unknown ids are rejected as 422.
    #[serde(default)]
    pub trip_retailers: Option<Vec<Retailer>>,
}

/// Plain-language reasons for one published plan (issue #103).
///
/// `plan` names the response field this explanation belongs to (`recommended`,
/// `best_single`, `best_two_store` or `theoretical_minimum`); `reasons` is
/// ordered most-important-first and already rendered to English, so the UI can
/// print the first few without re-deriving anything.
#[derive(Debug, Serialize)]
pub struct PlanExplanation {
    /// Which plan card these reasons belong to.
    pub plan: PlanKind,
    /// Ordered, human-readable reasons (two to four are shown per card).
    pub reasons: Vec<String>,
}

/// Response for `POST /api/optimise`: the four plans, all submitted item
/// identities, the resolved/unfulfilled item lists, the generated explanation,
/// the `run_id`, and the snapshot date.
#[derive(Debug, Serialize)]
pub struct OptimiseResponse {
    /// The persisted `recommendation_runs.id`, for correlation with later
    /// savings events.
    pub run_id: String,
    /// Cheapest single-store plan (or `null` if nothing could be priced).
    pub best_single: Option<StorePlan>,
    /// Cheapest two-store split (or `null`).
    pub best_two_store: Option<StorePlan>,
    /// Every item at its globally cheapest store — a benchmark, never routed.
    pub theoretical_minimum: Option<StorePlan>,
    /// The practical recommendation (or `null`).
    pub recommended: Option<Recommendation>,
    /// Identity and input order for every active list entry sent to the optimiser.
    pub basket_items: Vec<BasketItemIdentity>,
    /// Items with `resolution_status != 'resolved'` (excluded from every total).
    pub unresolved_items: Vec<UnresolvedItem>,
    /// Resolved items that produced no priceable offer anywhere.
    pub no_valid_offer_items: Vec<NoValidOfferItem>,
    /// Resolved, priceable items held back for household confirmation because
    /// their `substitution_policy = 'ask'` (Backlog-T1). Excluded from every plan
    /// and from all totals until the household chooses a product. Each carries its
    /// `canonical_item_id` and the constraint-passing `candidates` (issue #78) so
    /// the UI can render a chooser and confirm one via `POST /api/match-feedback`
    /// (`prefer`); the chosen product then enters the plans on the next optimise.
    pub needs_confirmation_items: Vec<NeedsConfirmationItem>,
    /// Deterministic, human-readable summary naming which stores are cheaper for
    /// which categories (not produced by the core; generated here).
    pub plain_language_explanation: String,
    /// Per-plan plain-language reasons (issue #103), ordered most-important-first.
    /// The UI renders two to four of each plan's reasons on its card.
    pub plan_explanations: Vec<PlanExplanation>,
    /// Latest `source_snapshot_date` among the offers backing the recommended
    /// plan (nullable: offers may not carry a snapshot date).
    pub offer_snapshot_date: Option<String>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/optimise` — optimise a stored list and persist a run.
///
/// Status discipline mirrors the sibling handlers (`lists.rs`): 401 without a
/// live session (the extractor), 403 if CSRF is missing/invalid (the extractor)
/// or the caller is not a member of the list's household, 404 if the list does
/// not exist, 422 when the *effective* (post-default) `threshold_3 < threshold_2`,
/// and 500 (logged, never failing open) on a database error. The body itself is
/// validated by the `Json` extractor: a malformed JSON body is rejected as 400,
/// a well-formed body of the wrong shape (e.g. missing `list_id`) as 422.
async fn optimise_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<OptimiseRequest>,
) -> Result<Json<OptimiseResponse>, StatusCode> {
    let pool = &state.pool;

    // Look the list up first: a missing list is 404, an existing list the caller
    // is not a member of is 403 (deliberately distinct, as in `get_list`). The
    // household must be known before the effective thresholds are resolved, since
    // an omitted threshold falls back to that household's stored settings (P4-T2).
    let household_id = load_list_household(pool, &body.list_id)
        .await
        .map_err(db_error("optimise_list.load_list"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    if !is_member(pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("optimise_list.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // Effective thresholds: the request value, else the household's stored
    // settings (P4-T2), else the documented core defaults if no settings row
    // exists. The household_settings row is back-filled for every household by
    // migration 005, so the constant fallback only fires for a household created
    // after the back-fill without its own settings row.
    let (stored_2, stored_3) = crate::settings::stored_thresholds(pool, &household_id)
        .await
        .map_err(db_error("optimise_list.settings"))?
        .unwrap_or(DEFAULT_THRESHOLDS);
    let threshold_2 = body.convenience_threshold_2.unwrap_or(stored_2);
    let threshold_3 = body.convenience_threshold_3.unwrap_or(stored_3);

    // Validate the *effective* thresholds (post-fallback), not just the explicit
    // pair: a fallback filling one side (e.g. `{convenience_threshold_3: 500}`
    // with an implicit stored threshold_2 = €10) must not bypass these guards.
    // Thresholds are cents, so they must be non-negative — a negative explicit
    // threshold would let an equally-complete, MORE-expensive two-store split
    // clear the core's `saving > threshold_2` test — and a third-store threshold
    // can never be easier to justify than a second. `threshold_2 >= 0` plus the
    // ordering guard implies `threshold_3 >= 0`.
    if threshold_2 < 0 || threshold_3 < threshold_2 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Household store settings (issue #98): which retailers take part in plans
    // and which are nearby. Retailers missing a row default to active/not-nearby
    // in the core, preserving pre-#98 behaviour. `trip_retailers` admits the
    // household's occasional stores into this run only.
    let store_settings = crate::settings::stored_store_settings(pool, &household_id)
        .await
        .map_err(db_error("optimise_list.store_settings"))?;
    let trip_retailers = body.trip_retailers.clone().unwrap_or_default();

    let input = build_input_from_list(
        pool,
        &household_id,
        &body.list_id,
        threshold_2,
        store_settings,
        trip_retailers,
    )
    .await
    .map_err(db_error("optimise_list.build_input"))?;

    let output = optimise(pool, &input)
        .await
        .map_err(db_error("optimise_list.optimise"))?;

    // Derive the snapshot date and the explanation from the plans. Both read
    // category / offer metadata the core output does not carry.
    let offer_snapshot_date = latest_offer_snapshot_date(pool, &output)
        .await
        .map_err(db_error("optimise_list.snapshot_date"))?;
    let explanation = build_explanation(pool, &input, &output)
        .await
        .map_err(db_error("optimise_list.explanation"))?;
    let run_id = persist_run(
        pool,
        &body.list_id,
        &household_id,
        &output,
        offer_snapshot_date.as_deref(),
        threshold_2,
        threshold_3,
    )
    .await
    .map_err(db_error("optimise_list.persist"))?;

    Ok(Json(OptimiseResponse {
        run_id,
        best_single: output.best_single,
        best_two_store: output.best_two_store,
        theoretical_minimum: output.theoretical_minimum,
        recommended: output.recommended,
        basket_items: output.basket_items,
        unresolved_items: output.unresolved_items,
        no_valid_offer_items: output.no_valid_offer_items,
        needs_confirmation_items: output.needs_confirmation_items,
        plain_language_explanation: explanation.summary,
        plan_explanations: explanation.plans,
        offer_snapshot_date,
    }))
}

// ---------------------------------------------------------------------------
// Authorization helpers (mirroring lists.rs)
// ---------------------------------------------------------------------------

/// The household owning a list, or `None` if no such list exists.
async fn load_list_household(
    pool: &sqlx::SqlitePool,
    list_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT household_id FROM shopping_lists WHERE id = ?1")
        .bind(list_id)
        .fetch_optional(pool)
        .await
}

/// True if `user_id` is a member of `household_id` (via `household_members`).
///
/// A DB error is propagated so callers fail closed (500), never treating a
/// transient failure as "not a member". Mirrors `lists::is_member`.
async fn is_member(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    household_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM household_members WHERE user_id = ?1 AND household_id = ?2",
    )
    .bind(user_id)
    .bind(household_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Map a DB error to a logged 500, so a transient failure never fails open.
///
/// Generic over the error type so it covers both the raw `sqlx::Error` returned
/// by the local auth helpers and the `DbError` returned by the core reads.
fn db_error<E: std::fmt::Display>(context: &'static str) -> impl Fn(E) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "optimise: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ---------------------------------------------------------------------------
// List -> OptimiserInput (the API pre-step)
// ---------------------------------------------------------------------------

/// Build an [`OptimiserInput`] from a list's ACTIVE entries.
///
/// The list entries are already resolved (P1/P3), so — unlike the CLI's
/// `build_optimiser_input`, which resolves raw fixture text — this reads
/// `resolution_status` + `canonical_item_id` straight off each entry. For a
/// resolved entry it loads the same `default_constraint_policy` and
/// `item_constraints` row the CLI loads. The CLI then merges a per-line requested
/// size (`merge_requested_size`) parsed from raw text; the API has no per-line
/// size to merge (`list_entries` stores none), so that step is a no-op here and
/// the produced `BasketItem` matches the CLI's for an already-resolved item.
///
/// An entry whose `resolution_status != 'resolved'` (unresolved or ambiguous)
/// carries no `canonical_item_id`, so the optimiser routes it to
/// `unresolved_items` — satisfying the acceptance criterion that
/// `unresolved_items` lists exactly those entries.
async fn build_input_from_list(
    pool: &sqlx::SqlitePool,
    household_id: &str,
    list_id: &str,
    threshold_2_cents: Cents,
    store_settings: Vec<StoreSetting>,
    trip_retailers: Vec<Retailer>,
) -> Result<OptimiserInput, DbError> {
    // ACTIVE entries only, ordered for determinism (matches lists.rs ordering).
    let rows = sqlx::query_as::<_, (String, String, Option<String>, String, i64)>(
        "SELECT id, resolution_status, canonical_item_id, display_name, quantity \
         FROM list_entries \
         WHERE list_id = ?1 AND status = 'active' \
         ORDER BY created_at, id",
    )
    .bind(list_id)
    .fetch_all(pool)
    .await?;

    // Normalise each entry to its priceable canonical id up front. Only a fully
    // resolved entry is priceable; ambiguous/unresolved entries drop their
    // canonical id so the optimiser surfaces them as unresolved. The entry id is
    // carried through so the optimiser can echo it back — the client maps a
    // priced/excluded line back to the household's list entry for the
    // per-store trip view (issue #104).
    let entries: Vec<(String, Option<String>, String, i64)> = rows
        .into_iter()
        .map(|(id, status, canonical_item_id, display_name, quantity)| {
            let canonical_item_id = (status == "resolved")
                .then_some(canonical_item_id)
                .flatten();
            (id, canonical_item_id, display_name, quantity)
        })
        .collect();

    // Bulk-load policies + constraints for every distinct resolved id in 1-2
    // chunked IN(...) queries each, avoiding a per-entry N+1 round-trip.
    let ids: Vec<&str> = entries
        .iter()
        .filter_map(|(_, canonical_item_id, _, _)| canonical_item_id.as_deref())
        .collect();
    let policies = canonical_item_policies(pool, &ids).await?;
    let constraints_by_id = item_constraints_by_canonical_ids(pool, &ids).await?;

    let mut items = Vec::with_capacity(entries.len());
    for (entry_id, canonical_item_id, display_name, quantity) in entries {
        let (policy, constraints) = match canonical_item_id.as_deref() {
            Some(id) => (
                policies.get(id).copied().unwrap_or(ConstraintPolicy::Any),
                constraints_by_id.get(id).cloned(),
            ),
            None => (ConstraintPolicy::Any, None),
        };

        items.push(BasketItem {
            // The household's list entry, echoed back so the client can map a
            // plan line to the entry it fulfils (issue #104 trip view).
            entry_id: Some(entry_id),
            // Display the stored entry text so the household recognises the line.
            display_text: display_name,
            canonical_item_id,
            // `quantity` is a positive count in the schema; clamp to at least 1
            // (matching `apply_add_item`). A count of 0 would let a resolved item
            // look fulfilled while contributing €0 — an incorrect optimisation.
            count: quantity.max(1) as u32,
            constraint_policy: policy,
            constraints,
        });
    }

    // Freshness needs the current BasketWatch run number (None => Unknown).
    let current_run =
        db::current_source_run_number(pool, buywise_core::domain::Source::BasketWatch).await?;

    let mut input = OptimiserInput::new(household_id, current_run, items);
    input.threshold_2_cents = threshold_2_cents;
    input.store_settings = store_settings;
    input.trip_retailers = trip_retailers;
    Ok(input)
}

/// Bulk-load `default_constraint_policy` for every given canonical item id, keyed
/// by id, in chunked `IN (...)` queries.
///
/// The map omits unknown ids; callers fall back to [`ConstraintPolicy::Any`] for a
/// missing key, exactly as the single-row `db::canonical_item_policy` does.
async fn canonical_item_policies(
    pool: &sqlx::SqlitePool,
    ids: &[&str],
) -> Result<std::collections::HashMap<String, ConstraintPolicy>, DbError> {
    let mut by_id = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(by_id);
    }
    for chunk in ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT id, default_constraint_policy FROM canonical_items WHERE id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, (String, ConstraintPolicy)>(&sql);
        for id in chunk {
            query = query.bind(*id);
        }
        for (id, policy) in query.fetch_all(pool).await? {
            by_id.insert(id, policy);
        }
    }
    Ok(by_id)
}

/// Bulk-load the full [`ItemConstraints`] for every given canonical item id,
/// keyed by id, in chunked `IN (...)` queries.
///
/// Backlog-T1: reads and parses exactly the columns
/// `db::item_constraints_by_canonical_id` does (`brand`, the
/// `allowed_brands`/`excluded_brands`/`dietary_flags` JSON lists, `pack_count`,
/// `form_factor`, `preferred_size`, `substitution_policy`), so a `BasketItem`
/// built from this map is byte-for-byte what the single-row helper would
/// produce. Ids without a row are simply absent.
async fn item_constraints_by_canonical_ids(
    pool: &sqlx::SqlitePool,
    ids: &[&str],
) -> Result<std::collections::HashMap<String, buywise_core::domain::ItemConstraints>, DbError> {
    use buywise_core::domain::{ItemConstraints, SizeBasis, SubstitutionPolicy};

    // (canonical_item_id, id, brand, allowed_brands, excluded_brands,
    // preferred_size_value, preferred_size_basis, pack_count, form_factor,
    // dietary_flags, substitution_policy, updated_at) — the keyed analogue of the
    // single-row read. JSON-list columns are read as raw TEXT and decoded below.
    type KeyedConstraintsRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<f64>,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<String>,
        String,
        String,
    );

    // Decode a JSON-array TEXT list leniently: NULL or unparseable → None.
    fn parse_list(raw: Option<&str>) -> Option<Vec<String>> {
        raw.and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
    }

    let mut by_id = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(by_id);
    }
    for chunk in ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT canonical_item_id, id, brand, allowed_brands, excluded_brands, \
                    preferred_size_value, preferred_size_basis, pack_count, \
                    form_factor, dietary_flags, substitution_policy, updated_at \
             FROM item_constraints WHERE canonical_item_id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, KeyedConstraintsRow>(&sql);
        for id in chunk {
            query = query.bind(*id);
        }
        for (
            canonical_item_id,
            id,
            brand,
            allowed_brands,
            excluded_brands,
            size_value,
            size_basis,
            pack_count,
            form_factor,
            dietary_flags,
            sub_policy,
            updated_at,
        ) in query.fetch_all(pool).await?
        {
            // Parse the enum TEXT columns leniently, matching the single-row helper:
            // an unparseable size basis is treated as absent, an unparseable
            // substitution policy falls back to the permissive default.
            let preferred_size_basis = size_basis
                .as_deref()
                .and_then(|s| SizeBasis::from_db_str(s).ok());
            let substitution_policy =
                SubstitutionPolicy::from_db_str(&sub_policy).unwrap_or(SubstitutionPolicy::Any);

            by_id.insert(
                canonical_item_id.clone(),
                ItemConstraints {
                    id,
                    canonical_item_id,
                    brand,
                    allowed_brands: parse_list(allowed_brands.as_deref()),
                    excluded_brands: parse_list(excluded_brands.as_deref()),
                    preferred_size_value: size_value,
                    preferred_size_basis,
                    min_size_value: None,
                    min_size_basis: None,
                    max_size_value: None,
                    max_size_basis: None,
                    pack_count,
                    form_factor,
                    dietary_flags: parse_list(dietary_flags.as_deref()),
                    substitution_policy,
                    updated_at,
                },
            );
        }
    }
    Ok(by_id)
}

// ---------------------------------------------------------------------------
// offer_snapshot_date
// ---------------------------------------------------------------------------

/// The latest `source_snapshot_date` among the offers backing the recommended
/// plan's lines, or `None` if no plan exists or none of its offers carry a date.
///
/// The plan lines carry `offer_id`; the snapshot date lives on `product_offers`
/// and is not echoed onto [`buywise_core::optimiser::PlanLine`], so it is fetched
/// here. ISO-8601 dates sort lexicographically, so `MAX(...)` returns the latest.
///
/// Scope note: the date is derived *only* from the offers backing the recommended
/// plan, not from every offer the optimiser considered. It is `None` when there is
/// no recommended plan or none of its offers carry a snapshot date.
async fn latest_offer_snapshot_date(
    pool: &sqlx::SqlitePool,
    output: &OptimiserOutput,
) -> Result<Option<String>, DbError> {
    let Some(plan) = output.recommended.as_ref().map(|r| &r.plan) else {
        return Ok(None);
    };
    if plan.lines.is_empty() {
        return Ok(None);
    }

    // Chunk the IN(...) by IN_CHUNK_SIZE so a plan with more offer ids than
    // SQLite's bound-parameter limit cannot turn a valid call into a 500. ISO-8601
    // dates sort lexicographically, so the overall latest is the max of the
    // per-chunk maxima.
    let offer_ids: Vec<&str> = plan.lines.iter().map(|l| l.offer_id.as_str()).collect();
    let mut latest: Option<String> = None;
    for chunk in offer_ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT MAX(source_snapshot_date) FROM product_offers WHERE id IN ({placeholders})"
        );
        let mut query = sqlx::query_scalar::<_, Option<String>>(&sql);
        for id in chunk {
            query = query.bind(*id);
        }
        let chunk_max: Option<String> = query.fetch_one(pool).await?;
        if chunk_max > latest {
            latest = chunk_max;
        }
    }
    Ok(latest)
}

// ---------------------------------------------------------------------------
// plain_language_explanation
// ---------------------------------------------------------------------------

/// The deterministic explanation payload: the one-paragraph summary plus the
/// per-plan reasons (issue #103).
struct Explanation {
    /// The existing `plain_language_explanation` summary paragraph.
    summary: String,
    /// Rendered reasons grouped by plan, in card order.
    plans: Vec<PlanExplanation>,
}

/// Build the deterministic plain-language explanation and the per-plan reasons.
///
/// The summary is the original cheaper-for paragraph: the acceptance criterion
/// requires the text to mention which stores are cheaper for which categories. We
/// derive that from the *theoretical minimum* plan — the only plan that prices
/// every item at its globally cheapest store, so its per-line retailer is exactly
/// "the cheapest store for this item". Grouping those lines by the item's
/// category yields, per category, the set of stores that win cheapest there; we
/// name them. The summary then states the recommended plan: its store(s), total,
/// and the saving versus the best single store.
///
/// The per-plan reasons extend that same path: the core emits structured,
/// plan-specific [`PlanReason`]s (issue #103) and this function renders each to
/// English using the category names and retailer labels, so both the summary and
/// the cards come from one deterministic source. Everything is sourced from the
/// plans + categories, so it is fully deterministic and testable.
async fn build_explanation(
    pool: &sqlx::SqlitePool,
    input: &OptimiserInput,
    output: &OptimiserOutput,
) -> Result<Explanation, DbError> {
    // Category names keyed by canonical_item_id, for every resolved basket item.
    let categories = category_names_by_item(pool, input).await?;

    let mut out = String::new();

    // Per-category cheaper-for phrasing, derived from the theoretical minimum.
    if let Some(theo) = output.theoretical_minimum.as_ref() {
        // category name -> ordered, de-duplicated list of winning retailers.
        let mut by_category: BTreeMap<String, Vec<Retailer>> = BTreeMap::new();
        for line in &theo.lines {
            let Some(item) = input.items.get(line.item_index) else {
                continue;
            };
            let Some(canonical_id) = item.canonical_item_id.as_deref() else {
                continue;
            };
            let category = categories
                .get(canonical_id)
                .cloned()
                .unwrap_or_else(|| "other".to_string());
            let retailers = by_category.entry(category).or_default();
            if !retailers.contains(&line.retailer) {
                retailers.push(line.retailer);
            }
        }

        let phrases: Vec<String> = by_category
            .iter()
            .map(|(category, retailers)| {
                let stores = join_stores(retailers);
                format!("{stores} {} cheaper for {category}", verb_for(retailers))
            })
            .collect();

        if !phrases.is_empty() {
            out.push_str(&capitalise_first(&phrases.join("; ")));
            out.push('.');
        }
    }

    // Recommended plan: store(s), total, and saving vs best single.
    if let Some(rec) = output.recommended.as_ref() {
        if !out.is_empty() {
            out.push(' ');
        }
        let stores = join_stores(&rec.plan.stores);
        out.push_str(&format!(
            "Recommended: shop at {stores} for a total of {}",
            euros(rec.plan.total_cents)
        ));
        if let Some(best_single) = output.best_single.as_ref() {
            out.push_str(&recommendation_comparison(&rec.plan, best_single));
        }
        out.push('.');
    } else {
        // No priceable plan at all (e.g. nothing resolved or no offers).
        if out.is_empty() {
            out.push_str("No priceable plan: no list item could be priced at any store.");
        }
    }

    // Per-plan reasons, rendered from the same category lookup as the summary.
    let plans = render_plan_explanations(&output.reasons, input, &categories);
    Ok(Explanation {
        summary: out,
        plans,
    })
}

fn recommendation_comparison(recommended: &StorePlan, best_single: &StorePlan) -> String {
    let recommended_items: BTreeSet<_> = recommended
        .lines
        .iter()
        .map(|line| line.item_index)
        .collect();
    let single_items: BTreeSet<_> = best_single
        .lines
        .iter()
        .map(|line| line.item_index)
        .collect();
    if recommended_items != single_items {
        return ", the recommendation covers a different set of items, so its total is not directly comparable with the best single store".to_string();
    }

    let saving = (best_single.total_cents - recommended.total_cents).max(0);
    if saving > 0 {
        format!(", saving {} versus the best single store", euros(saving))
    } else {
        ", the cheapest single store overall".to_string()
    }
}

/// Render the core's structured [`PlanReason`]s into per-plan English sentences.
///
/// Reasons are grouped by [`PlanKind`] in card order. Choice, coverage, missing
/// price, and freshness facts are combined or prioritized before the four-card
/// limit is applied.
fn render_plan_explanations(
    reasons: &[PlanReason],
    input: &OptimiserInput,
    categories: &std::collections::HashMap<String, String>,
) -> Vec<PlanExplanation> {
    const ORDER: [PlanKind; 4] = [
        PlanKind::Recommended,
        PlanKind::BestSingle,
        PlanKind::BestTwoStore,
        PlanKind::TheoreticalMinimum,
    ];
    let mut plans = Vec::new();
    for kind in ORDER {
        let plan_reasons: Vec<&PlanReason> = reasons.iter().filter(|r| r.plan == kind).collect();
        let mut rendered = Vec::new();

        let choice = plan_reasons.iter().copied().find(|reason| {
            matches!(
                reason.code,
                ReasonCode::ThresholdCleared
                    | ReasonCode::ThresholdNotCleared
                    | ReasonCode::SplitDropsItems
                    | ReasonCode::MoreComplete
                    | ReasonCode::CoverageDiffers
            )
        });
        if let Some(choice) = choice {
            rendered.push(render_reason(choice, input, categories));
        }

        let coverage_and_gaps = plan_reasons
            .iter()
            .filter(|reason| {
                matches!(
                    reason.code,
                    ReasonCode::Coverage
                        | ReasonCode::NoKnownPriceHere
                        | ReasonCode::NoKnownPriceAnywhere
                        | ReasonCode::UnresolvedItems
                )
            })
            .map(|reason| render_reason(reason, input, categories))
            .collect::<Vec<_>>();
        if !coverage_and_gaps.is_empty() {
            rendered.push(coverage_and_gaps.join("; "));
        }

        let freshness = plan_reasons
            .iter()
            .filter(|reason| matches!(reason.code, ReasonCode::PriceAsOf | ReasonCode::StalePrices))
            .map(|reason| render_reason(reason, input, categories))
            .collect::<Vec<_>>();
        if !freshness.is_empty() {
            rendered.push(freshness.join("; "));
        }

        let routing = plan_reasons
            .iter()
            .filter(|reason| {
                matches!(
                    reason.code,
                    ReasonCode::CheapestAtStore
                        | ReasonCode::OnlyStoreStocking
                        | ReasonCode::BrandRule
                )
            })
            .map(|reason| render_reason(reason, input, categories))
            .collect::<Vec<_>>();
        let alternatives = plan_reasons
            .iter()
            .filter(|reason| {
                matches!(
                    reason.code,
                    ReasonCode::RejectedStore | ReasonCode::CoverageDiffers
                ) && !choice.is_some_and(|chosen| std::ptr::eq(chosen, **reason))
            })
            .map(|reason| render_reason(reason, input, categories))
            .collect::<Vec<_>>();
        let routing_and_alternatives = match (routing.is_empty(), alternatives.is_empty()) {
            (false, false) => Some(format!(
                "{}; Alternatives considered: {}",
                routing.join("; "),
                alternatives.join("; ")
            )),
            (false, true) => Some(routing.join("; ")),
            (true, false) => Some(format!(
                "Alternatives considered: {}",
                alternatives.join("; ")
            )),
            (true, true) => None,
        };
        if let Some(reason) = routing_and_alternatives {
            rendered.push(reason);
        }

        for reason in &plan_reasons {
            if rendered.len() == 4 {
                break;
            }
            if matches!(
                reason.code,
                ReasonCode::ThresholdCleared
                    | ReasonCode::ThresholdNotCleared
                    | ReasonCode::SplitDropsItems
                    | ReasonCode::MoreComplete
                    | ReasonCode::CoverageDiffers
                    | ReasonCode::Coverage
                    | ReasonCode::NoKnownPriceHere
                    | ReasonCode::NoKnownPriceAnywhere
                    | ReasonCode::UnresolvedItems
                    | ReasonCode::PriceAsOf
                    | ReasonCode::StalePrices
                    | ReasonCode::CheapestAtStore
                    | ReasonCode::OnlyStoreStocking
                    | ReasonCode::BrandRule
                    | ReasonCode::RejectedStore
            ) {
                continue;
            }
            rendered.push(render_reason(reason, input, categories));
        }
        rendered.truncate(4);
        if !rendered.is_empty() {
            plans.push(PlanExplanation {
                plan: kind,
                reasons: rendered,
            });
        }
    }
    plans
}

/// Render one structured reason to a plain-language sentence.
///
/// Every sentence is built only from the reason's own facts (`item_indices`,
/// `retailers`, `cents`, `threshold_cents`, `snapshot_date`) plus the category
/// names, so it cannot describe anything the plan's lines do not support.
fn render_reason(
    reason: &PlanReason,
    input: &OptimiserInput,
    categories: &std::collections::HashMap<String, String>,
) -> String {
    let items = reason.item_indices.len();
    let suffix = category_suffix(&category_names_for(reason, input, categories));
    match reason.code {
        ReasonCode::CheapestAtStore => format!(
            "{} is cheapest for {items} item{}{suffix}",
            store_names(&reason.retailers),
            plural(items)
        ),
        ReasonCode::OnlyStoreStocking => format!(
            "{} is the only store we have a price for on {items} item{}{suffix}",
            store_names(&reason.retailers),
            plural(items)
        ),
        ReasonCode::BrandRule => format!("{items} item{} held to your brand rule", plural(items)),
        ReasonCode::Coverage => {
            let total = input.items.len();
            let amount = reason.cents.map(euros).unwrap_or_else(|| "€?".into());
            if items >= total {
                format!(
                    "Prices all {total} basket item{} for {amount}",
                    plural(total)
                )
            } else {
                format!("Prices {items} of {total} basket items for {amount}")
            }
        }
        ReasonCode::NoKnownPriceHere => {
            // Never imply the item is unavailable: our data may simply lack a
            // qualifying price, which is not proof of no stock.
            let unavailable = if items == 1 { "it is" } else { "they are" };
            format!(
                "No known price at {} for {items} item{}{suffix} - missing data, not proof {unavailable} unavailable",
                store_names(&reason.retailers),
                plural(items)
            )
        }
        ReasonCode::NoKnownPriceAnywhere => format!(
            "No qualifying price on record anywhere for {items} item{}{suffix}",
            plural(items)
        ),
        ReasonCode::UnresolvedItems => {
            format!("Could not match {items} item{} to a product", plural(items))
        }
        ReasonCode::StalePrices => format!(
            "Prices for {items} item{} at {} are not from the latest run",
            plural(items),
            store_names(&reason.retailers)
        ),
        ReasonCode::PriceAsOf => match reason.snapshot_date.as_deref() {
            Some(date) if !reason.item_indices.is_empty() => format!(
                "Prices as of {date}; no snapshot date recorded for {items} item{}",
                plural(items)
            ),
            Some(date) => format!("Prices as of {date}"),
            None => "Prices have no recorded snapshot date".to_string(),
        },
        ReasonCode::ThresholdCleared => {
            let bar = reason.threshold_cents.map(euros).unwrap_or_default();
            let nearby = nearby_suffix(reason, input);
            let saving = reason.cents.map(euros).unwrap_or_default();
            format!(
                "Adding {} saves {saving}, above your {bar}{nearby} bar",
                store_names(&reason.retailers)
            )
        }
        ReasonCode::ThresholdNotCleared => {
            let bar = reason.threshold_cents.map(euros).unwrap_or_default();
            let nearby = nearby_suffix(reason, input);
            let saving = reason.cents.map(euros).unwrap_or_default();
            format!(
                "Adding {} would save {saving}, below your {bar}{nearby} bar, so one store is enough",
                store_names(&reason.retailers)
            )
        }
        ReasonCode::SplitDropsItems => {
            format!(
                "Adding {} would leave {items} item{} unpriced, so the single store wins",
                store_names(&reason.retailers),
                plural(items)
            )
        }
        ReasonCode::MoreComplete => format!(
            "{} also covers {items} item{} one store cannot, so the extra stop earns its place",
            store_names(&reason.retailers),
            plural(items)
        ),
        ReasonCode::RejectedStore => match reason.cents {
            Some(delta) if delta > 0 => format!(
                "Shopping at {} alone would save {}",
                store_names(&reason.retailers),
                euros(delta)
            ),
            Some(0) => format!(
                "Shopping at {} alone would cost the same",
                store_names(&reason.retailers)
            ),
            Some(delta) => format!(
                "Shopping at {} alone would cost {} more",
                store_names(&reason.retailers),
                euros(-delta)
            ),
            None => format!(
                "{} was considered as a single store",
                store_names(&reason.retailers)
            ),
        },
        ReasonCode::CoverageDiffers => format!(
            "{} alone prices a different set of items, so its total is not directly comparable",
            store_names(&reason.retailers)
        ),
        ReasonCode::StoreOff => format!(
            "{} is switched off in your store settings",
            store_names(&reason.retailers)
        ),
        ReasonCode::StoreNotOnTrip => format!(
            "{} is occasional and is not marked for today",
            store_names(&reason.retailers)
        ),
    }
}

/// The category names behind a reason's items, de-duplicated and in line order.
fn category_names_for(
    reason: &PlanReason,
    input: &OptimiserInput,
    categories: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for &index in &reason.item_indices {
        let Some(item) = input.items.get(index) else {
            continue;
        };
        let Some(canonical_id) = item.canonical_item_id.as_deref() else {
            continue;
        };
        let Some(name) = categories.get(canonical_id) else {
            continue;
        };
        if !names.iter().any(|n| n == name) {
            names.push(name.clone());
        }
    }
    names
}

/// `" (Dairy, Bakery)"` for a reason's categories, capped at three with a count
/// of the rest so a long basket cannot produce an unreadable line.
fn category_suffix(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1..=3 => format!(" ({})", names.join(", ")),
        _ => format!(" ({}, and {} more)", names[..3].join(", "), names.len() - 3),
    }
}

/// Mark a threshold reason as using the lower nearby-store bar: the only way the
/// effective bar falls below the household's stored threshold.
fn nearby_suffix(reason: &PlanReason, input: &OptimiserInput) -> &'static str {
    match reason.threshold_cents {
        Some(bar) if bar < input.threshold_2_cents => " nearby-store",
        _ => "",
    }
}

/// `"s"` for any count other than one.
fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Comma-joined retailer labels for a reason's stores.
fn store_names(retailers: &[Retailer]) -> String {
    retailers
        .iter()
        .map(|r| store_label(*r))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Look up the English category name for every resolved basket item, keyed by
/// `canonical_item_id`. One chunked query joins `canonical_items -> categories`.
async fn category_names_by_item(
    pool: &sqlx::SqlitePool,
    input: &OptimiserInput,
) -> Result<std::collections::HashMap<String, String>, DbError> {
    let ids: Vec<&str> = input
        .items
        .iter()
        .filter_map(|i| i.canonical_item_id.as_deref())
        .collect();
    let mut by_item = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(by_item);
    }

    for chunk in ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT ci.id, c.name_en \
             FROM canonical_items ci JOIN categories c ON c.id = ci.category_id \
             WHERE ci.id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, (String, String)>(&sql);
        for id in chunk {
            query = query.bind(*id);
        }
        for (id, name) in query.fetch_all(pool).await? {
            by_item.insert(id, name);
        }
    }
    Ok(by_item)
}

/// Human-readable join of retailer names: `"Aldi"`, `"Aldi and Tesco"`,
/// `"Aldi, Tesco and Lidl"`.
fn join_stores(retailers: &[Retailer]) -> String {
    let names: Vec<String> = retailers.iter().map(|r| store_label(*r)).collect();
    match names.as_slice() {
        [] => "no store".to_string(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// "is" for a single store, "are" for several (subject-verb agreement).
fn verb_for(retailers: &[Retailer]) -> &'static str {
    if retailers.len() == 1 {
        "is"
    } else {
        "are"
    }
}

/// Display label for a retailer (title-cased, matching the brand spelling).
fn store_label(retailer: Retailer) -> String {
    match retailer {
        Retailer::Tesco => "Tesco",
        Retailer::Dunnes => "Dunnes",
        Retailer::SuperValu => "SuperValu",
        Retailer::Aldi => "Aldi",
        Retailer::Lidl => "Lidl",
    }
    .to_string()
}

/// Format integer cents as a euro string, e.g. `1234 -> "€12.34"`.
fn euros(cents: Cents) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}€{}.{:02}", abs / 100, abs % 100)
}

/// Capitalise the first character of `s` (ASCII-safe; category names are ASCII).
fn capitalise_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// recommendation_runs persistence
// ---------------------------------------------------------------------------

/// All four plans, serialised together for the `recommendation_runs.plans_json`
/// column. Mirrors the optimiser's plan vocabulary so the stored JSON is
/// self-describing for later readers (P4-T3 / savings ledger).
#[derive(Serialize)]
struct PlansJson<'a> {
    best_single: Option<&'a StorePlan>,
    best_two_store: Option<&'a StorePlan>,
    theoretical_minimum: Option<&'a StorePlan>,
    recommended: Option<&'a Recommendation>,
}

/// Insert one `recommendation_runs` row and return its id.
///
/// `id` is a UUID v4 (matching `lists.rs`/`mutations.rs`); `run_at` is RFC3339
/// (matching every other timestamped row via `db::now_iso`). `plans_json` carries
/// all four plans; `recommended_plan` is the `RecommendedKind` serde string
/// (`single_store`/`two_store`), or `"none"` when no plan exists. Both effective
/// thresholds (post-default) are persisted.
async fn persist_run(
    pool: &sqlx::SqlitePool,
    list_id: &str,
    household_id: &str,
    output: &OptimiserOutput,
    offer_snapshot_date: Option<&str>,
    threshold_2: Cents,
    threshold_3: Cents,
) -> Result<String, DbError> {
    let id = uuid::Uuid::new_v4().to_string();
    let run_at = db::now_iso();

    let plans = PlansJson {
        best_single: output.best_single.as_ref(),
        best_two_store: output.best_two_store.as_ref(),
        theoretical_minimum: output.theoretical_minimum.as_ref(),
        recommended: output.recommended.as_ref(),
    };
    // serde_json on owned, already-validated data cannot fail in practice; treat
    // a failure as a 500 via DbError rather than panicking.
    let plans_json = serde_json::to_string(&plans)
        .map_err(|e| DbError::from(sqlx::Error::Protocol(format!("serialise plans_json: {e}"))))?;

    let recommended_plan = match output.recommended.as_ref().map(|r| r.kind) {
        Some(RecommendedKind::SingleStore) => "single_store",
        Some(RecommendedKind::TwoStore) => "two_store",
        None => "none",
    };

    sqlx::query(
        "INSERT INTO recommendation_runs \
             (id, list_id, household_id, run_at, offer_snapshot_date, plans_json, \
              recommended_plan, convenience_threshold_2, convenience_threshold_3) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&id)
    .bind(list_id)
    .bind(household_id)
    .bind(&run_at)
    .bind(offer_snapshot_date)
    .bind(&plans_json)
    .bind(recommended_plan)
    .bind(threshold_2)
    .bind(threshold_3)
    .execute(pool)
    .await?;

    Ok(id)
}

// ---------------------------------------------------------------------------
// Small SQL helpers (mirroring lists.rs)
// ---------------------------------------------------------------------------

/// Max bound parameters per chunked `IN (...)` query (see `lists.rs`).
const IN_CHUNK_SIZE: usize = 900;

/// Build `n` comma-separated `?` placeholders for an `IN (...)` clause.
fn sql_placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n.saturating_mul(2));
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use buywise_core::optimiser::BasketItem;
    use std::collections::HashMap;

    /// A resolved basket item with no constraints.
    fn basket_item(canonical_id: &str) -> BasketItem {
        BasketItem {
            entry_id: None,
            display_text: canonical_id.to_string(),
            canonical_item_id: Some(canonical_id.to_string()),
            count: 1,
            constraint_policy: ConstraintPolicy::Any,
            constraints: None,
        }
    }

    /// An input with three items across two categories.
    fn input() -> OptimiserInput {
        OptimiserInput::new(
            "hh-1",
            Some(1),
            vec![
                basket_item("ci-milk"),
                basket_item("ci-cheese"),
                basket_item("ci-bread"),
            ],
        )
    }

    /// Category names for [`input`]: milk/cheese are Dairy, bread is Bakery.
    fn categories() -> HashMap<String, String> {
        HashMap::from([
            ("ci-milk".to_string(), "Dairy".to_string()),
            ("ci-cheese".to_string(), "Dairy".to_string()),
            ("ci-bread".to_string(), "Bakery".to_string()),
        ])
    }

    fn plan_with_items(item_indices: &[usize], total_cents: Cents) -> StorePlan {
        StorePlan {
            stores: vec![Retailer::Tesco],
            lines: item_indices
                .iter()
                .map(|&item_index| buywise_core::optimiser::PlanLine {
                    item_index,
                    entry_id: None,
                    display_text: format!("item {item_index}"),
                    product_name: format!("product {item_index}"),
                    canonical_item_id: Some(format!("item-{item_index}")),
                    retailer: Retailer::Tesco,
                    retailer_product_id: format!("product-{item_index}"),
                    offer_id: format!("offer-{item_index}"),
                    unit_price_cents: 100,
                    line_total_cents: 100,
                    loyalty_applied: false,
                    freshness_confidence: buywise_core::domain::FreshnessConfidence::High,
                })
                .collect(),
            total_cents,
            missing_count: 0,
            dunnes_voucher_offer_text: None,
        }
    }

    /// A reason with the given code and nothing else set.
    fn bare(plan: PlanKind, code: ReasonCode) -> PlanReason {
        PlanReason {
            plan,
            code,
            item_indices: Vec::new(),
            retailers: Vec::new(),
            cents: None,
            threshold_cents: None,
            snapshot_date: None,
        }
    }

    #[test]
    fn cheapest_at_store_names_the_store_count_and_categories() {
        let reason = PlanReason {
            item_indices: vec![0, 1],
            retailers: vec![Retailer::Aldi],
            ..bare(PlanKind::BestSingle, ReasonCode::CheapestAtStore)
        };
        assert_eq!(
            render_reason(&reason, &input(), &categories()),
            "Aldi is cheapest for 2 items (Dairy)"
        );
    }

    #[test]
    fn coverage_reports_partial_and_full_baskets() {
        let partial = PlanReason {
            item_indices: vec![0, 1],
            cents: Some(320),
            ..bare(PlanKind::BestSingle, ReasonCode::Coverage)
        };
        assert_eq!(
            render_reason(&partial, &input(), &categories()),
            "Prices 2 of 3 basket items for €3.20"
        );

        let full = PlanReason {
            item_indices: vec![0, 1, 2],
            cents: Some(500),
            ..bare(PlanKind::BestSingle, ReasonCode::Coverage)
        };
        assert_eq!(
            render_reason(&full, &input(), &categories()),
            "Prices all 3 basket items for €5.00"
        );
    }

    #[test]
    fn recommendation_comparison_requires_identical_priced_items() {
        let best_single = plan_with_items(&[0, 1], 3_000);

        let more_complete = plan_with_items(&[0, 1, 2], 2_800);
        assert_eq!(
            recommendation_comparison(&more_complete, &best_single),
            ", the recommendation covers a different set of items, so its total is not directly comparable with the best single store"
        );

        let costlier_more_complete = plan_with_items(&[0, 1, 2], 3_200);
        assert_eq!(
            recommendation_comparison(&costlier_more_complete, &best_single),
            ", the recommendation covers a different set of items, so its total is not directly comparable with the best single store"
        );

        let same_items = plan_with_items(&[1, 0], 2_800);
        assert_eq!(
            recommendation_comparison(&same_items, &best_single),
            ", saving €2.00 versus the best single store"
        );
    }

    #[test]
    fn no_known_price_never_claims_the_item_is_unavailable() {
        // Singular and plural: neither sentence may assert unavailability.
        for (items, expected) in [
            (
                vec![0],
                "No known price at Tesco for 1 item (Dairy) - missing data, not proof it is unavailable",
            ),
            (
                vec![0, 1],
                "No known price at Tesco for 2 items (Dairy) - missing data, not proof they are unavailable",
            ),
        ] {
            let reason = PlanReason {
                item_indices: items,
                retailers: vec![Retailer::Tesco],
                ..bare(PlanKind::BestSingle, ReasonCode::NoKnownPriceHere)
            };
            let text = render_reason(&reason, &input(), &categories());
            assert_eq!(text, expected);
            assert!(
                !text.contains("not available"),
                "must not claim unavailability: {text}"
            );
        }
    }

    #[test]
    fn threshold_reasons_name_the_store_saving_and_bar() {
        let reason = PlanReason {
            retailers: vec![Retailer::Tesco],
            cents: Some(999),
            threshold_cents: Some(1000),
            ..bare(PlanKind::Recommended, ReasonCode::ThresholdNotCleared)
        };
        assert_eq!(
            render_reason(&reason, &input(), &categories()),
            "Adding Tesco would save €9.99, below your €10.00 bar, so one store is enough"
        );

        let cleared = PlanReason {
            retailers: vec![Retailer::Tesco],
            cents: Some(1200),
            threshold_cents: Some(1000),
            ..bare(PlanKind::Recommended, ReasonCode::ThresholdCleared)
        };
        assert_eq!(
            render_reason(&cleared, &input(), &categories()),
            "Adding Tesco saves €12.00, above your €10.00 bar"
        );
    }

    #[test]
    fn nearby_bar_is_called_out_when_it_lowered_the_threshold() {
        let reason = PlanReason {
            retailers: vec![Retailer::Tesco],
            cents: Some(700),
            threshold_cents: Some(500),
            ..bare(PlanKind::Recommended, ReasonCode::ThresholdCleared)
        };
        let text = render_reason(&reason, &input(), &categories());
        assert!(text.contains("€5.00 nearby-store bar"), "got: {text}");
    }

    #[test]
    fn rejected_store_reports_a_saving_or_an_overspend() {
        let cheaper = PlanReason {
            retailers: vec![Retailer::Tesco],
            cents: Some(250),
            ..bare(PlanKind::Recommended, ReasonCode::RejectedStore)
        };
        assert_eq!(
            render_reason(&cheaper, &input(), &categories()),
            "Shopping at Tesco alone would save €2.50"
        );

        let costlier = PlanReason {
            retailers: vec![Retailer::Tesco],
            cents: Some(-250),
            ..bare(PlanKind::Recommended, ReasonCode::RejectedStore)
        };
        assert_eq!(
            render_reason(&costlier, &input(), &categories()),
            "Shopping at Tesco alone would cost €2.50 more"
        );

        let tied = PlanReason {
            retailers: vec![Retailer::Tesco],
            cents: Some(0),
            ..bare(PlanKind::Recommended, ReasonCode::RejectedStore)
        };
        assert_eq!(
            render_reason(&tied, &input(), &categories()),
            "Shopping at Tesco alone would cost the same"
        );
    }

    #[test]
    fn price_as_of_reports_the_snapshot_date() {
        let reason = PlanReason {
            snapshot_date: Some("2026-06-01".to_string()),
            ..bare(PlanKind::BestSingle, ReasonCode::PriceAsOf)
        };
        assert_eq!(
            render_reason(&reason, &input(), &categories()),
            "Prices as of 2026-06-01"
        );
    }

    #[test]
    fn price_as_of_mentions_undated_lines() {
        let reason = PlanReason {
            item_indices: vec![0, 1],
            snapshot_date: Some("2026-05-20".to_string()),
            ..bare(PlanKind::BestSingle, ReasonCode::PriceAsOf)
        };
        assert_eq!(
            render_reason(&reason, &input(), &categories()),
            "Prices as of 2026-05-20; no snapshot date recorded for 2 items"
        );
    }

    #[test]
    fn groups_rendered_reasons_by_plan_in_card_order() {
        // Deliberately out of order, with two reasons for one plan to prove
        // intra-plan ordering is preserved.
        let reasons = vec![
            PlanReason {
                retailers: vec![Retailer::Tesco],
                ..bare(PlanKind::BestTwoStore, ReasonCode::Coverage)
            },
            PlanReason {
                item_indices: vec![0],
                ..bare(PlanKind::Recommended, ReasonCode::Coverage)
            },
            PlanReason {
                item_indices: vec![0],
                retailers: vec![Retailer::Aldi],
                ..bare(PlanKind::Recommended, ReasonCode::CheapestAtStore)
            },
            PlanReason {
                ..bare(PlanKind::BestSingle, ReasonCode::Coverage)
            },
        ];
        let plans = render_plan_explanations(&reasons, &input(), &categories());
        let kinds: Vec<PlanKind> = plans.iter().map(|p| p.plan).collect();
        assert_eq!(
            kinds,
            vec![
                PlanKind::Recommended,
                PlanKind::BestSingle,
                PlanKind::BestTwoStore
            ]
        );
        assert_eq!(plans[0].reasons.len(), 2);
        assert_eq!(plans[0].reasons[0], "Prices 1 of 3 basket items for €?");
        assert_eq!(plans[0].reasons[1], "Aldi is cheapest for 1 item (Dairy)");
    }

    #[test]
    fn card_reasons_keep_choice_coverage_gaps_and_freshness_with_many_candidates() {
        let mut basket = input();
        basket.items[2].canonical_item_id = None;
        basket.threshold_2_cents = 400;
        let reasons = vec![
            PlanReason {
                retailers: vec![Retailer::Aldi],
                cents: Some(500),
                threshold_cents: Some(400),
                ..bare(PlanKind::Recommended, ReasonCode::ThresholdCleared)
            },
            PlanReason {
                item_indices: vec![0],
                retailers: vec![Retailer::Tesco],
                ..bare(PlanKind::Recommended, ReasonCode::NoKnownPriceHere)
            },
            PlanReason {
                item_indices: vec![1],
                ..bare(PlanKind::Recommended, ReasonCode::NoKnownPriceAnywhere)
            },
            PlanReason {
                item_indices: vec![2],
                ..bare(PlanKind::Recommended, ReasonCode::UnresolvedItems)
            },
            PlanReason {
                item_indices: vec![0, 1],
                cents: Some(300),
                ..bare(PlanKind::Recommended, ReasonCode::Coverage)
            },
            PlanReason {
                ..bare(PlanKind::Recommended, ReasonCode::PriceAsOf)
            },
            PlanReason {
                item_indices: vec![0, 1],
                retailers: vec![Retailer::Aldi, Retailer::Tesco],
                ..bare(PlanKind::Recommended, ReasonCode::StalePrices)
            },
            PlanReason {
                item_indices: vec![0],
                retailers: vec![Retailer::Aldi],
                ..bare(PlanKind::Recommended, ReasonCode::CheapestAtStore)
            },
        ];

        let plans = render_plan_explanations(&reasons, &basket, &categories());
        let card = &plans[0].reasons;
        assert_eq!(card.len(), 4);
        assert!(card[0].contains("above your €4.00 bar"), "{card:?}");
        assert!(card[1].contains("Prices 2 of 3 basket items"));
        assert!(card[1].contains("No known price at Tesco"));
        assert!(card[1].contains("No qualifying price on record anywhere"));
        assert!(card[1].contains("Could not match 1 item"));
        assert!(card[2].contains("Prices have no recorded snapshot date"));
        assert!(card[2].contains("not from the latest run"));
        assert!(card[2].contains("at Aldi, Tesco"));
    }

    #[test]
    fn recommended_split_reserves_routing_and_merges_rejected_stores() {
        let reasons = vec![
            PlanReason {
                retailers: vec![Retailer::Tesco],
                cents: Some(1500),
                threshold_cents: Some(1000),
                ..bare(PlanKind::Recommended, ReasonCode::ThresholdCleared)
            },
            PlanReason {
                item_indices: vec![0, 1, 2],
                cents: Some(1000),
                ..bare(PlanKind::Recommended, ReasonCode::Coverage)
            },
            PlanReason {
                snapshot_date: Some("2026-06-01".to_string()),
                ..bare(PlanKind::Recommended, ReasonCode::PriceAsOf)
            },
            PlanReason {
                item_indices: vec![0],
                retailers: vec![Retailer::Aldi],
                ..bare(PlanKind::Recommended, ReasonCode::CheapestAtStore)
            },
            PlanReason {
                item_indices: vec![1],
                retailers: vec![Retailer::Tesco],
                ..bare(PlanKind::Recommended, ReasonCode::OnlyStoreStocking)
            },
            PlanReason {
                item_indices: vec![2],
                ..bare(PlanKind::Recommended, ReasonCode::BrandRule)
            },
            PlanReason {
                retailers: vec![Retailer::Dunnes],
                cents: Some(200),
                ..bare(PlanKind::Recommended, ReasonCode::RejectedStore)
            },
            PlanReason {
                retailers: vec![Retailer::Lidl],
                cents: Some(-125),
                ..bare(PlanKind::Recommended, ReasonCode::RejectedStore)
            },
            PlanReason {
                retailers: vec![Retailer::SuperValu],
                ..bare(PlanKind::Recommended, ReasonCode::CoverageDiffers)
            },
        ];

        let plans = render_plan_explanations(&reasons, &input(), &categories());
        let card = &plans[0].reasons;
        assert_eq!(card.len(), 4);
        assert!(card[0].contains("above your €10.00 bar"));
        assert!(card[1].contains("Prices all 3 basket items"));
        assert!(card[2].contains("Prices as of 2026-06-01"));
        assert!(card[3].contains("Aldi is cheapest for 1 item"));
        assert!(card[3].contains("Tesco is the only store we have a price for"));
        assert!(card[3].contains("1 item held to your brand rule"));
        assert!(card[3].contains("Shopping at Dunnes alone would save €2.00"));
        assert!(card[3].contains("Shopping at Lidl alone would cost €1.25 more"));
        assert!(card[3].contains("SuperValu alone prices a different set of items"));
        assert_eq!(
            card.iter()
                .filter(|line| line.contains("Shopping at"))
                .count(),
            1
        );
        assert!(
            card[3].find("Aldi is cheapest") < card[3].find("Alternatives considered"),
            "routing should precede rejected-store notes: {card:?}"
        );
    }
}
