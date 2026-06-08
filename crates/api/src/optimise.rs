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
//! ([`buywise_core::optimiser::DEFAULT_THRESHOLD_2_CENTS`] for threshold-2 and the
//! €25/2500 [`DEFAULT_THRESHOLD_3_CENTS`] for threshold-3). Phase-0/Phase-4 plans
//! are single- and two-store only, so threshold-3 has no effect on the plans
//! today; it is accepted and persisted for forward-compatibility (and validated:
//! a third store can never be easier to justify than a second).

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::db::{self, DbError};
use buywise_core::domain::{Cents, ConstraintPolicy, Retailer};
use buywise_core::optimiser::{
    optimise, BasketItem, NoValidOfferItem, OptimiserInput, OptimiserOutput, Recommendation,
    RecommendedKind, StorePlan, UnresolvedItem,
};

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Documented default third-store convenience threshold (cents): €25.00.
///
/// The core exposes a threshold-2 default
/// ([`buywise_core::optimiser::DEFAULT_THRESHOLD_2_CENTS`]) but no threshold-3
/// constant (Phase-0/Phase-4 produce no three-store plans). When neither the
/// request nor the household's `household_settings` row (P4-T2) supplies a
/// threshold-3, it falls back to this documented default
/// (`docs/implementation-plan.md`, P4-T2 acceptance criteria: "€25 (2500 cents)
/// for threshold_3").
pub const DEFAULT_THRESHOLD_3_CENTS: Cents = 2500;

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
}

/// Response for `POST /api/optimise`: the four plans, the resolved/unfulfilled
/// item lists, the generated explanation, the `run_id`, and the snapshot date.
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
    /// Items with `resolution_status != 'resolved'` (excluded from every total).
    pub unresolved_items: Vec<UnresolvedItem>,
    /// Resolved items that produced no priceable offer anywhere.
    pub no_valid_offer_items: Vec<NoValidOfferItem>,
    /// Deterministic, human-readable summary naming which stores are cheaper for
    /// which categories (not produced by the core; generated here).
    pub plain_language_explanation: String,
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
        .unwrap_or(crate::settings::DEFAULT_THRESHOLDS);
    let threshold_2 = body.convenience_threshold_2.unwrap_or(stored_2);
    let threshold_3 = body.convenience_threshold_3.unwrap_or(stored_3);

    // A third-store threshold can never be easier to justify than a second.
    // Validate the *effective* thresholds (post-fallback), not just the explicit
    // pair: a fallback filling one side (e.g. `{convenience_threshold_3: 500}`
    // with an implicit stored threshold_2 = €10) must not bypass the ordering
    // guard.
    if threshold_3 < threshold_2 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let input = build_input_from_list(pool, &household_id, &body.list_id, threshold_2)
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
        unresolved_items: output.unresolved_items,
        no_valid_offer_items: output.no_valid_offer_items,
        plain_language_explanation: explanation,
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
) -> Result<OptimiserInput, DbError> {
    // ACTIVE entries only, ordered for determinism (matches lists.rs ordering).
    let rows = sqlx::query_as::<_, (String, Option<String>, String, i64)>(
        "SELECT resolution_status, canonical_item_id, display_name, quantity \
         FROM list_entries \
         WHERE list_id = ?1 AND status = 'active' \
         ORDER BY created_at, id",
    )
    .bind(list_id)
    .fetch_all(pool)
    .await?;

    // Normalise each entry to its priceable canonical id up front. Only a fully
    // resolved entry is priceable; ambiguous/unresolved entries drop their
    // canonical id so the optimiser surfaces them as unresolved.
    let entries: Vec<(Option<String>, String, i64)> = rows
        .into_iter()
        .map(|(status, canonical_item_id, display_name, quantity)| {
            let canonical_item_id = (status == "resolved")
                .then_some(canonical_item_id)
                .flatten();
            (canonical_item_id, display_name, quantity)
        })
        .collect();

    // Bulk-load policies + constraints for every distinct resolved id in 1-2
    // chunked IN(...) queries each, avoiding a per-entry N+1 round-trip.
    let ids: Vec<&str> = entries
        .iter()
        .filter_map(|(id, _, _)| id.as_deref())
        .collect();
    let policies = canonical_item_policies(pool, &ids).await?;
    let constraints_by_id = item_constraints_by_canonical_ids(pool, &ids).await?;

    let mut items = Vec::with_capacity(entries.len());
    for (canonical_item_id, display_name, quantity) in entries {
        let (policy, constraints) = match canonical_item_id.as_deref() {
            Some(id) => (
                policies.get(id).copied().unwrap_or(ConstraintPolicy::Any),
                constraints_by_id.get(id).cloned(),
            ),
            None => (ConstraintPolicy::Any, None),
        };

        items.push(BasketItem {
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

/// Bulk-load the Phase-0-enforced [`ItemConstraints`] for every given canonical
/// item id, keyed by id, in chunked `IN (...)` queries.
///
/// Reads and parses exactly the columns `db::item_constraints_by_canonical_id`
/// does (`brand`, `preferred_size_value`/`preferred_size_basis`,
/// `substitution_policy`), so a `BasketItem` built from this map is byte-for-byte
/// what the single-row helper would produce. Ids without a row are simply absent.
async fn item_constraints_by_canonical_ids(
    pool: &sqlx::SqlitePool,
    ids: &[&str],
) -> Result<std::collections::HashMap<String, buywise_core::domain::ItemConstraints>, DbError> {
    use buywise_core::domain::{ItemConstraints, SizeBasis, SubstitutionPolicy};

    // (canonical_item_id, id, brand, preferred_size_value, preferred_size_basis,
    // substitution_policy, updated_at) — the keyed analogue of the single-row read.
    type KeyedConstraintsRow = (
        String,
        String,
        Option<String>,
        Option<f64>,
        Option<String>,
        String,
        String,
    );

    let mut by_id = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(by_id);
    }
    for chunk in ids.chunks(IN_CHUNK_SIZE) {
        let placeholders = sql_placeholders(chunk.len());
        let sql = format!(
            "SELECT canonical_item_id, id, brand, preferred_size_value, preferred_size_basis, \
                    substitution_policy, updated_at \
             FROM item_constraints WHERE canonical_item_id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, KeyedConstraintsRow>(&sql);
        for id in chunk {
            query = query.bind(*id);
        }
        for (canonical_item_id, id, brand, size_value, size_basis, sub_policy, updated_at) in
            query.fetch_all(pool).await?
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
                    allowed_brands: None,
                    excluded_brands: None,
                    preferred_size_value: size_value,
                    preferred_size_basis,
                    min_size_value: None,
                    min_size_basis: None,
                    max_size_value: None,
                    max_size_basis: None,
                    pack_count: None,
                    form_factor: None,
                    dietary_flags: None,
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

/// Build the deterministic plain-language explanation.
///
/// The acceptance criterion requires the text to mention which stores are cheaper
/// for which categories. We derive that from the *theoretical minimum* plan —
/// the only plan that prices every item at its globally cheapest store, so its
/// per-line retailer is exactly "the cheapest store for this item". Grouping
/// those lines by the item's category yields, per category, the set of stores
/// that win cheapest there; we name them.
///
/// The text then states the recommended plan: its store(s), total, and the saving
/// versus the best single store, so the household sees both the per-category
/// detail and the bottom line. Everything is sourced from the plans + a single
/// category lookup, so it is fully deterministic and testable.
async fn build_explanation(
    pool: &sqlx::SqlitePool,
    input: &OptimiserInput,
    output: &OptimiserOutput,
) -> Result<String, DbError> {
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
        let single = output.best_single.as_ref().map(|p| p.total_cents);
        if let Some(single_total) = single {
            let saving = (single_total - rec.plan.total_cents).max(0);
            if saving > 0 {
                out.push_str(&format!(
                    ", saving {} versus the best single store",
                    euros(saving)
                ));
            } else {
                out.push_str(", the cheapest single store overall");
            }
        }
        out.push('.');
    } else {
        // No priceable plan at all (e.g. nothing resolved or no offers).
        if out.is_empty() {
            out.push_str("No priceable plan: no list item could be priced at any store.");
        }
    }

    Ok(out)
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
