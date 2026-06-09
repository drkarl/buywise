//! Savings-ledger routes for the BuyWise API (P5-T3 #34, P5-T4 #35).
//!
//! Two endpoints: `POST /api/list/{id}/actual-spend`, which records the
//! household's actual post-shop spend against the list's `savings_events` row and
//! computes the realised `net_saving_cents`; and `GET /api/savings/summary`,
//! which aggregates `savings_events` into the household's savings-dashboard data
//! (monthly totals, a cumulative net figure, and per-list outcomes).
//!
//! The `savings_events` row itself is created when the list is archived (see
//! `lists::record_savings_event`); this endpoint only *updates* an existing row.
//! Posting actual spend for a list that never produced a savings event (no
//! recommendation run, or no usable plan) is a **404** — we never create a row
//! from actual spend, so a 404 cleanly signals "nothing to attribute spend to"
//! (consistent with how the list endpoints signal a missing resource).
//!
//! Authorization mirrors `lists::patch_list`: the [`AuthenticatedUser`] extractor
//! enforces a live session (401) and CSRF on this POST (403); the list is then
//! looked up (404 if missing) and the caller's household membership verified (403
//! for a non-member). `net_saving_cents` is computed as
//! `baseline_cost_cents − chosen_basket_cost_cents − travel_cost_cents` per the
//! issue contract (the simple baseline-vs-actual form; design.md's
//! `accepted − travel − subscription` generalisation is the later P5-T4 work).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::domain::Cents;

use crate::lists::is_member;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the savings routes as a `Router<AppState>`, merged by [`crate::app`].
///
/// Both paths are absolute so they merge alongside the other `/api/...` shapes
/// rather than nesting: `/api/list/{id}/actual-spend` (record post-shop spend,
/// P5-T3) and `/api/savings/summary` (the household savings dashboard, P5-T4).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/list/{id}/actual-spend", post(record_actual_spend))
        .route("/api/savings/summary", get(savings_summary))
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// Body for `POST /api/list/{id}/actual-spend`.
///
/// Both fields are cents and must be non-negative (negative spend or travel is a
/// 422). `travel_cost_cents` defaults to `0` when omitted.
#[derive(Debug, Deserialize)]
pub struct ActualSpendRequest {
    /// The actual basket cost the household paid (cents). Stored as
    /// `chosen_basket_cost_cents`.
    pub actual_spend_cents: Cents,
    /// Extra travel cost for the trip (cents); subtracted from the net saving.
    #[serde(default)]
    pub travel_cost_cents: Cents,
}

/// Response for `POST /api/list/{id}/actual-spend`: the updated ledger figures.
#[derive(Debug, Serialize)]
pub struct ActualSpendResponse {
    /// `savings_events.id`.
    pub id: String,
    /// `savings_events.list_id`.
    pub list_id: String,
    /// The anchor cost the saving is measured against (best single store).
    pub baseline_cost_cents: Cents,
    /// The actual spend just recorded (`chosen_basket_cost_cents`).
    pub chosen_basket_cost_cents: Cents,
    /// The travel cost just recorded.
    pub travel_cost_cents: Cents,
    /// `net_saving_cents` = baseline − chosen_basket − travel.
    pub net_saving_cents: Cents,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/list/{id}/actual-spend` — record actual spend and compute the net
/// saving for the list's existing `savings_events` row.
///
/// Status discipline mirrors `lists::patch_list`: 401 without a session and 403
/// for a missing/invalid CSRF token (the extractor); 422 for a negative spend or
/// travel cost (validated at the boundary, before any lookup); 404 if the list
/// does not exist; 403 if the caller is not a member of the owning household; 404
/// again if the list exists but has no `savings_events` row (nothing to attribute
/// spend to — we never create a row here); 500 (logged, never failing open) on a
/// database error.
async fn record_actual_spend(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<ActualSpendRequest>,
) -> Result<Json<ActualSpendResponse>, StatusCode> {
    // Validate the body at the boundary first: cents must be non-negative.
    if body.actual_spend_cents < 0 || body.travel_cost_cents < 0 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Look the list up: missing list is 404, non-member is 403 (the same
    // distinction `patch_list`/`get_list` draw).
    let household_id: Option<String> =
        sqlx::query_scalar("SELECT household_id FROM shopping_lists WHERE id = ?1")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_error("actual_spend.load_list"))?;
    let household_id = household_id.ok_or(StatusCode::NOT_FOUND)?;

    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("actual_spend.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // The savings_events row must already exist (created on archive). Read the
    // baseline so the net saving can be computed; a missing row is a clean 404.
    let row: Option<(String, Cents)> = sqlx::query_as(
        "SELECT id, baseline_cost_cents FROM savings_events WHERE list_id = ?1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error("actual_spend.load_event"))?;
    let (event_id, baseline_cost_cents) = row.ok_or(StatusCode::NOT_FOUND)?;

    // net = baseline − actual − travel (the issue's two-field contract).
    let net_saving_cents = baseline_cost_cents - body.actual_spend_cents - body.travel_cost_cents;

    sqlx::query(
        "UPDATE savings_events \
         SET chosen_basket_cost_cents = ?2, travel_cost_cents = ?3, net_saving_cents = ?4 \
         WHERE id = ?1",
    )
    .bind(&event_id)
    .bind(body.actual_spend_cents)
    .bind(body.travel_cost_cents)
    .bind(net_saving_cents)
    .execute(&state.pool)
    .await
    .map_err(db_error("actual_spend.update"))?;

    Ok(Json(ActualSpendResponse {
        id: event_id,
        list_id: id,
        baseline_cost_cents,
        chosen_basket_cost_cents: body.actual_spend_cents,
        travel_cost_cents: body.travel_cost_cents,
        net_saving_cents,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/savings/summary (P5-T4)
// ---------------------------------------------------------------------------

/// One calendar month's aggregate savings, derived from `savings_events`.
///
/// `month` is `"YYYY-MM"` (UTC, off `created_at`). `gross_cents` is the sum of
/// the always-present `gross_saving_cents`. `accepted_cents` is **derived** as
/// `baseline_cost_cents − chosen_basket_cost_cents` summed over the month's
/// reconciled rows only (those with a non-null `chosen_basket_cost_cents`);
/// `net_cents` sums `net_saving_cents` treating NULL as 0. Both are only non-zero
/// once the household enters their actual post-shop spend, so a month of
/// un-reconciled lists shows €0 accepted/net (not a skewed total). `lists` is the
/// count of savings events (= archived lists with a saving) in the month.
///
/// Note: the schema's `accepted_savings_cents` / `subscription_cost_alloc_cents`
/// columns are not populated this phase, so accepted is derived from the stored
/// baseline/chosen figures rather than read from that column.
#[derive(Debug, Serialize)]
pub struct MonthlySummary {
    /// Calendar month as `"YYYY-MM"`.
    pub month: String,
    /// Sum of `gross_saving_cents` (always present) for the month.
    pub gross_cents: Cents,
    /// Derived accepted saving: sum of `baseline − chosen_basket` over the
    /// month's reconciled (non-null `chosen_basket_cost_cents`) rows; 0 otherwise.
    pub accepted_cents: Cents,
    /// Sum of `net_saving_cents` (NULL treated as 0) for the month.
    pub net_cents: Cents,
    /// Number of savings events (archived lists with a saving) in the month.
    pub lists: i64,
}

/// One archived list's savings outcome, for the dashboard's per-list table.
///
/// `chosen_basket_cost_cents`/`net_saving_cents` are NULL until the household
/// posts their actual spend; the UI uses a null `chosen_basket_cost_cents` to
/// show the "Enter actual spend" prompt (AC3). Ordered newest-first.
#[derive(Debug, Serialize)]
pub struct ListOutcome {
    /// `savings_events.id`.
    pub id: String,
    /// The owning list's id (for the actual-spend link).
    pub list_id: String,
    /// The list's display name.
    pub list_name: String,
    /// `savings_events.created_at` (ISO-8601 UTC) — when the list was archived.
    pub created_at: String,
    /// Always-present gross saving for the list.
    pub gross_saving_cents: Cents,
    /// Actual basket cost the household paid, NULL until actual-spend is posted.
    pub chosen_basket_cost_cents: Option<Cents>,
    /// Realised net saving, NULL until actual-spend is posted.
    pub net_saving_cents: Option<Cents>,
}

/// Response for `GET /api/savings/summary`.
///
/// `monthly` and `cumulative_net_cents` are the issue's authoritative contract;
/// `lists` is an additive per-list breakdown the dashboard renders as a table.
/// Per the acceptance criteria, `cumulative_net_cents` is the SUM of
/// `net_saving_cents` over the non-null rows only (un-reconciled lists don't
/// count toward the cumulative figure).
#[derive(Debug, Serialize)]
pub struct SavingsSummaryResponse {
    /// Per-month aggregates, ordered chronologically (oldest first).
    pub monthly: Vec<MonthlySummary>,
    /// Sum of `net_saving_cents` across all non-null rows.
    pub cumulative_net_cents: Cents,
    /// Per-archived-list outcomes, newest first.
    pub lists: Vec<ListOutcome>,
}

/// `GET /api/savings/summary` — the caller's household savings dashboard data:
/// monthly aggregates, a cumulative net figure, and per-list outcomes.
///
/// Scoping: a user may belong to more than one household, but this endpoint has
/// no `household_id` parameter, so it resolves the caller's single household the
/// same deterministic way `GET /api/settings` does (`caller_household`:
/// `ORDER BY household_id LIMIT 1`) and restricts every figure to that one
/// household. This keeps the dashboard a single household's view rather than a
/// blend across all of the caller's memberships, and one household never sees
/// another's savings. 404 if the caller belongs to no household (mirrors
/// `GET /api/settings`); 401 without a session (the extractor); 500 (logged) on
/// a database error. A household with no archived lists yields empty
/// `monthly`/`lists` and a `0` cumulative — never an error.
async fn savings_summary(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<SavingsSummaryResponse>, StatusCode> {
    // Resolve the caller's single household deterministically, mirroring the
    // `GET /api/settings` precedent (`settings::caller_household`). No household
    // membership is a 404, consistent with that sibling endpoint.
    let household_id = caller_household(&state.pool, &user.user_id)
        .await
        .map_err(db_error("savings_summary.household"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Per-month aggregates. `accepted_cents` is DERIVED as baseline − chosen over
    // reconciled rows only (the schema's `accepted_savings_cents` column is never
    // populated this phase); an un-reconciled row (chosen NULL) contributes 0 to
    // both accepted and net, so a month of un-reconciled lists reads as €0 rather
    // than skewing the total. This keeps accepted internally consistent with the
    // stored net (= baseline − chosen − travel): accepted = net + travel for
    // reconciled rows. Gross is NOT NULL in the schema; strftime works on the
    // ISO-8601 text `created_at`.
    let monthly = sqlx::query_as::<_, (String, Cents, Cents, Cents, i64)>(
        "SELECT strftime('%Y-%m', se.created_at)        AS month, \
                SUM(se.gross_saving_cents)              AS gross_cents, \
                SUM(CASE WHEN se.chosen_basket_cost_cents IS NOT NULL \
                         THEN se.baseline_cost_cents - se.chosen_basket_cost_cents \
                         ELSE 0 END)                    AS accepted_cents, \
                SUM(COALESCE(se.net_saving_cents, 0))   AS net_cents, \
                COUNT(*)                                AS lists \
         FROM savings_events se \
         JOIN shopping_lists sl ON sl.id = se.list_id \
         WHERE sl.household_id = ?1 \
         GROUP BY month \
         ORDER BY month",
    )
    .bind(&household_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("savings_summary.monthly"))?
    .into_iter()
    .map(
        |(month, gross_cents, accepted_cents, net_cents, lists)| MonthlySummary {
            month,
            gross_cents,
            accepted_cents,
            net_cents,
            lists,
        },
    )
    .collect();

    // Cumulative net: sum of net_saving_cents over NON-NULL rows only (AC2). A
    // pure SUM already ignores NULLs; COALESCE wraps the all-NULL case to 0.
    let cumulative_net_cents: Cents = sqlx::query_scalar(
        "SELECT COALESCE(SUM(se.net_saving_cents), 0) \
         FROM savings_events se \
         JOIN shopping_lists sl ON sl.id = se.list_id \
         WHERE sl.household_id = ?1",
    )
    .bind(&household_id)
    .fetch_one(&state.pool)
    .await
    .map_err(db_error("savings_summary.cumulative"))?;

    // Per-list outcomes, newest first, for the dashboard table.
    let lists = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            Cents,
            Option<Cents>,
            Option<Cents>,
        ),
    >(
        "SELECT se.id, se.list_id, sl.name, se.created_at, se.gross_saving_cents, \
                se.chosen_basket_cost_cents, se.net_saving_cents \
         FROM savings_events se \
         JOIN shopping_lists sl ON sl.id = se.list_id \
         WHERE sl.household_id = ?1 \
         ORDER BY se.created_at DESC, se.id DESC",
    )
    .bind(&household_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("savings_summary.lists"))?
    .into_iter()
    .map(
        |(
            id,
            list_id,
            list_name,
            created_at,
            gross_saving_cents,
            chosen_basket_cost_cents,
            net_saving_cents,
        )| {
            ListOutcome {
                id,
                list_id,
                list_name,
                created_at,
                gross_saving_cents,
                chosen_basket_cost_cents,
                net_saving_cents,
            }
        },
    )
    .collect();

    Ok(Json(SavingsSummaryResponse {
        monthly,
        cumulative_net_cents,
        lists,
    }))
}

/// The (single) household the caller belongs to, or `None` if they belong to no
/// household.
///
/// Mirrors `settings::caller_household`: a user may belong to several households,
/// but the savings summary has no `household_id` parameter, so it resolves the
/// caller's first membership deterministically (`ORDER BY household_id LIMIT 1`).
/// Kept local to match the established `GET /api/settings` precedent without
/// coupling the two modules.
async fn caller_household(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT household_id FROM household_members WHERE user_id = ?1 \
         ORDER BY household_id LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// Map a DB error to a logged 500, so a transient failure never fails open.
/// Mirrors `lists::db_error`.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "savings: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}
