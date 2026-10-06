//! Savings-ledger routes for the BuyWise API (P5-T3 #34, P5-T4 #35, #117).
//!
//! Two endpoints: `POST /api/list/{id}/actual-spend`, which records the
//! household's actual post-shop spend against the list's `savings_events` row and
//! computes the realised **total-only basket variance**; and
//! `GET /api/savings/summary`, which aggregates `savings_events` into the
//! household's savings-dashboard data.
//!
//! **Honest labels (issue #117).** A saving is only a saving when the evidence
//! proves one, so every event carries a `status` (migration 016) and only the
//! reserved `verified` status may be called saved money:
//!
//!   * `verified` — line- and quantity-reconciled evidence. **Reserved**: never
//!     produced by this slice (it needs the purchase ledger of #116), so the
//!     dashboard's verified aggregate is empty today.
//!   * `estimated` — the best single store and the recommended plan price the
//!     same lines, so `baseline − recommended` is a like-for-like *opportunity
//!     estimate*. It is not a realised saving.
//!   * `unreconciled` — the two plans price different lines, so no saving is
//!     claimed. The stored gross figure is kept for audit but never presented or
//!     aggregated as saved money.
//!   * `legacy_total_only` — a manual whole-basket actual-spend total compared
//!     against the best single store estimate. Quantities and purchases are
//!     unverified; travel is included where entered; service and data costs are
//!     excluded. Shown as a legacy basket variance, never as saved money.
//!
//! The `savings_events` row itself is created when the list is archived (see
//! `lists::record_savings_event`, which stamps `estimated` / `unreconciled`);
//! this module's POST only *updates* an existing row and stamps
//! `legacy_total_only`. Posting actual spend for a list that never produced a
//! savings event (no recommendation run, or no usable plan) is a **404** — we
//! never create a row from actual spend, so a 404 cleanly signals "nothing to
//! attribute spend to".
//!
//! Authorization mirrors `lists::patch_list`: the [`AuthenticatedUser`] extractor
//! enforces a live session (401) and CSRF on this POST (403); the list is then
//! looked up (404 if missing) and the caller's household membership verified (403
//! for a non-member). The stored `net_saving_cents` is
//! `baseline_cost_cents − chosen_basket_cost_cents − travel_cost_cents`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::domain::Cents;

use crate::lists::is_member;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

// ---------------------------------------------------------------------------
// Event statuses and display copy
// ---------------------------------------------------------------------------

/// Line- and quantity-reconciled evidence. **Reserved**: never produced by this
/// slice, so no aggregate called saved or net savings can be non-empty yet.
pub const STATUS_VERIFIED: &str = "verified";
/// A comparable-coverage gross estimate with no actual spend yet. Both plans
/// price the same lines, so the difference is like-for-like — but it is an
/// opportunity estimate, not a realised saving.
pub const STATUS_ESTIMATED: &str = "estimated";
/// The plans price different lines, so the stored gross figure is not a saving
/// and is never aggregated as one.
pub const STATUS_UNRECONCILED: &str = "unreconciled";
/// A manual whole-basket actual-spend total: a total-only basket variance.
pub const STATUS_LEGACY_TOTAL_ONLY: &str = "legacy_total_only";

/// Short copy naming the baseline every current figure is measured against.
const BASELINE_LABEL: &str = "Baseline: best single store estimate";
/// Short copy naming which costs a realised figure includes.
const COSTS_NOTE: &str = "Travel included where entered; service and data costs excluded.";

/// Short human label for a stored status. Unknown values fail safe to a generic
/// unverified label rather than echoing raw data.
fn status_label(status: &str) -> &'static str {
    match status {
        STATUS_VERIFIED => "Verified saving",
        STATUS_ESTIMATED => "Estimated opportunity (not verified)",
        STATUS_UNRECONCILED => "Not comparable — plans price different lines",
        STATUS_LEGACY_TOTAL_ONLY => "Legacy total-only basket variance",
        _ => "Unverified",
    }
}

/// Short copy describing the evidence a stored status covers.
fn coverage_label(status: &str) -> &'static str {
    match status {
        STATUS_VERIFIED => "Lines and quantities reconciled",
        STATUS_ESTIMATED => "Both plans price the same lines; quantities not reconciled",
        STATUS_UNRECONCILED => "No saving claimed: the plans price different lines",
        STATUS_LEGACY_TOTAL_ONLY => "Whole-basket total only; quantities and purchases unverified",
        _ => "Coverage unknown",
    }
}

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
    /// Extra travel cost for the trip (cents); included in the basket variance.
    #[serde(default)]
    pub travel_cost_cents: Cents,
}

/// Response for `POST /api/list/{id}/actual-spend`: the updated ledger figures.
///
/// The difference is deliberately **not** called a net saving: it is a total-only
/// basket variance against the best single store estimate, with quantities and
/// purchases unverified.
#[derive(Debug, Serialize)]
pub struct ActualSpendResponse {
    /// `savings_events.id`.
    pub id: String,
    /// `savings_events.list_id`.
    pub list_id: String,
    /// Always `legacy_total_only` — a manual whole-basket total.
    pub status: String,
    /// The baseline the variance is measured against (best single store total).
    pub baseline_cost_cents: Cents,
    /// The actual spend just recorded (`chosen_basket_cost_cents`).
    pub chosen_basket_cost_cents: Cents,
    /// The travel cost just recorded.
    pub travel_cost_cents: Cents,
    /// `baseline − chosen_basket − travel`, a total-only basket variance.
    pub basket_variance_cents: Cents,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/list/{id}/actual-spend` — record actual spend and store the
/// total-only basket variance for the list's existing `savings_events` row.
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
    // baseline so the variance can be computed; a missing row is a clean 404.
    let row: Option<(String, Cents)> = sqlx::query_as(
        "SELECT id, baseline_cost_cents FROM savings_events WHERE list_id = ?1 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error("actual_spend.load_event"))?;
    let (event_id, baseline_cost_cents) = row.ok_or(StatusCode::NOT_FOUND)?;

    // variance = baseline − actual − travel (the issue's two-field contract).
    let basket_variance_cents =
        baseline_cost_cents - body.actual_spend_cents - body.travel_cost_cents;

    // A manual whole-basket total is always a `legacy_total_only` event: buying
    // fewer items (or extras) cannot be reconciled from one number, so it is
    // never counted as saved money.
    sqlx::query(
        "UPDATE savings_events \
         SET chosen_basket_cost_cents = ?2, travel_cost_cents = ?3, net_saving_cents = ?4, \
             status = ?5 \
         WHERE id = ?1",
    )
    .bind(&event_id)
    .bind(body.actual_spend_cents)
    .bind(body.travel_cost_cents)
    .bind(basket_variance_cents)
    .bind(STATUS_LEGACY_TOTAL_ONLY)
    .execute(&state.pool)
    .await
    .map_err(db_error("actual_spend.update"))?;

    Ok(Json(ActualSpendResponse {
        id: event_id,
        list_id: id,
        status: STATUS_LEGACY_TOTAL_ONLY.to_string(),
        baseline_cost_cents,
        chosen_basket_cost_cents: body.actual_spend_cents,
        travel_cost_cents: body.travel_cost_cents,
        basket_variance_cents,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/savings/summary (P5-T4, #117)
// ---------------------------------------------------------------------------

/// One calendar month's figures, grouped by the list's archive date (the date
/// the shop belongs to), formatted `"YYYY-MM"` (UTC). Only the events of one
/// status feed a bucket, so a bucket is labelled by the aggregate it belongs to.
///
/// `gross_cents` sums the stored `gross_saving_cents`; `accepted_cents` is
/// **derived** as `baseline_cost_cents − chosen_basket_cost_cents` over the
/// bucket's reconciled rows (chosen non-null); `net_cents` sums
/// `net_saving_cents` (NULL as 0). `lists` is the count of events in the bucket.
#[derive(Debug, Serialize)]
pub struct MonthlyBucket {
    /// Calendar month as `"YYYY-MM"`.
    pub month: String,
    /// Sum of the stored `gross_saving_cents` for the bucket.
    pub gross_cents: Cents,
    /// Derived `baseline − chosen_basket` over the bucket's reconciled rows.
    pub accepted_cents: Cents,
    /// Sum of `net_saving_cents` (NULL treated as 0) for the bucket.
    pub net_cents: Cents,
    /// Number of savings events in the bucket.
    pub lists: i64,
}

/// One aggregate of savings events sharing a status, with the short copy that
/// says what it is and what it covers.
#[derive(Debug, Serialize)]
pub struct SavingsAggregate {
    /// Short human label for the aggregate.
    pub label: String,
    /// One sentence saying exactly what the aggregate is (and is not).
    pub note: String,
    /// Short copy naming the baseline the figures compare against.
    pub baseline_label: String,
    /// Short copy naming which costs the figures include.
    pub costs_note: String,
    /// Sum of the bucket events' `net_saving_cents` (NULL treated as 0).
    pub cumulative_cents: Cents,
    /// Per-month buckets, oldest first.
    pub monthly: Vec<MonthlyBucket>,
}

/// One savings event's outcome, for the dashboard's per-event table.
///
/// `chosen_basket_cost_cents`/`basket_variance_cents` are NULL until the
/// household posts their actual spend; the UI uses a null
/// `chosen_basket_cost_cents` to show the "Enter actual spend" prompt. Ordered by
/// archive date, newest first.
#[derive(Debug, Serialize)]
pub struct ListOutcome {
    /// `savings_events.id`.
    pub id: String,
    /// The owning list's id (for the actual-spend link).
    pub list_id: String,
    /// The list's display name.
    pub list_name: String,
    /// The archive (shop) date, ISO-8601 UTC — the grouping date, never the row
    /// creation timestamp.
    pub archived_at: String,
    /// The stored status: `estimated`, `unreconciled`, or `legacy_total_only`
    /// (`verified` is reserved and never produced by this slice).
    pub status: String,
    /// Short human label for `status`.
    pub status_label: String,
    /// Short copy naming the baseline the figures compare against.
    pub baseline_label: String,
    /// Short copy describing the evidence the figures cover.
    pub coverage_label: String,
    /// Short copy naming which costs the figures include.
    pub costs_note: String,
    /// The stored gross figure (a like-for-like estimate, or an audit-only value
    /// when the row is `unreconciled`).
    pub gross_saving_cents: Cents,
    /// Actual basket cost the household paid, NULL until actual-spend is posted.
    pub chosen_basket_cost_cents: Option<Cents>,
    /// The total-only basket variance, NULL until actual-spend is posted.
    pub basket_variance_cents: Option<Cents>,
}

/// Response for `GET /api/savings/summary`.
///
/// `saved` is the only aggregate that may be called saved or net savings, and it
/// counts only `verified` events — so it is empty until line-level reconciliation
/// exists. `legacy` presents every total-only basket variance separately, under
/// its own label, with its original figures; it is never counted as saved money.
/// `lists` is the per-event breakdown the dashboard renders as a table.
#[derive(Debug, Serialize)]
pub struct SavingsSummaryResponse {
    /// The verified-only "saved" aggregate (empty today).
    pub saved: SavingsAggregate,
    /// The legacy total-only basket variance, shown separately.
    pub legacy: SavingsAggregate,
    /// Per-event outcomes, newest archive date first.
    pub lists: Vec<ListOutcome>,
}

/// `GET /api/savings/summary` — the caller's household savings dashboard data:
/// the verified-only saved aggregate, the separately labelled legacy total-only
/// variance, and per-event outcomes.
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
/// aggregates and `lists`, never an error.
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

    // Only `verified` events may be called saved money; none exist yet, so this
    // aggregate is honestly empty.
    let saved = aggregate_for(
        &state.pool,
        &household_id,
        STATUS_VERIFIED,
        "Verified savings",
        "Only line-reconciled results count as saved. There are none yet.",
    )
    .await
    .map_err(db_error("savings_summary.saved"))?;

    // Manual whole-basket totals are shown separately, with their original
    // figures, and are never counted as saved money.
    let legacy = aggregate_for(
        &state.pool,
        &household_id,
        STATUS_LEGACY_TOTAL_ONLY,
        "Legacy total-only basket variance",
        "Whole-basket actual spend recorded against the best single store estimate; \
         quantities and purchases unverified.",
    )
    .await
    .map_err(db_error("savings_summary.legacy"))?;

    // Per-event outcomes, newest archive date first.
    let lists = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            Cents,
            Option<Cents>,
            Option<Cents>,
        ),
    >(
        "SELECT se.id, se.list_id, sl.name, \
                COALESCE(sl.archived_at, se.created_at) AS archived_at, \
                se.status, se.gross_saving_cents, se.chosen_basket_cost_cents, \
                se.net_saving_cents \
         FROM savings_events se \
         JOIN shopping_lists sl ON sl.id = se.list_id \
         WHERE sl.household_id = ?1 \
         ORDER BY archived_at DESC, se.id DESC",
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
            archived_at,
            status,
            gross_saving_cents,
            chosen_basket_cost_cents,
            basket_variance_cents,
        )| ListOutcome {
            id,
            list_id,
            list_name,
            status_label: status_label(&status).to_string(),
            coverage_label: coverage_label(&status).to_string(),
            archived_at,
            status,
            baseline_label: BASELINE_LABEL.to_string(),
            costs_note: COSTS_NOTE.to_string(),
            gross_saving_cents,
            chosen_basket_cost_cents,
            basket_variance_cents,
        },
    )
    .collect();

    Ok(Json(SavingsSummaryResponse {
        saved,
        legacy,
        lists,
    }))
}

/// Aggregate every event of one `status` for a household: per-month buckets
/// (grouped and ordered by archive date, not row creation) plus a cumulative sum
/// of the stored `net_saving_cents`.
///
/// `accepted_cents` is DERIVED as `baseline − chosen` over the bucket's
/// reconciled rows only (the schema's `accepted_savings_cents` column is never
/// populated this phase), so a bucket of un-reconciled rows reads as €0 rather
/// than skewing the total. Gross is NOT NULL in the schema.
async fn aggregate_for(
    pool: &sqlx::SqlitePool,
    household_id: &str,
    status: &str,
    label: &str,
    note: &str,
) -> Result<SavingsAggregate, sqlx::Error> {
    let monthly = sqlx::query_as::<_, (String, Cents, Cents, Cents, i64)>(
        "SELECT strftime('%Y-%m', COALESCE(sl.archived_at, se.created_at)) AS month, \
                SUM(se.gross_saving_cents)              AS gross_cents, \
                SUM(CASE WHEN se.chosen_basket_cost_cents IS NOT NULL \
                         THEN se.baseline_cost_cents - se.chosen_basket_cost_cents \
                         ELSE 0 END)                    AS accepted_cents, \
                SUM(COALESCE(se.net_saving_cents, 0))   AS net_cents, \
                COUNT(*)                                AS lists \
         FROM savings_events se \
         JOIN shopping_lists sl ON sl.id = se.list_id \
         WHERE sl.household_id = ?1 AND se.status = ?2 \
         GROUP BY month \
         ORDER BY month",
    )
    .bind(household_id)
    .bind(status)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(
        |(month, gross_cents, accepted_cents, net_cents, lists)| MonthlyBucket {
            month,
            gross_cents,
            accepted_cents,
            net_cents,
            lists,
        },
    )
    .collect();

    // A pure SUM already ignores NULLs; COALESCE wraps the all-NULL case to 0.
    let cumulative_cents: Cents = sqlx::query_scalar(
        "SELECT COALESCE(SUM(se.net_saving_cents), 0) \
         FROM savings_events se \
         JOIN shopping_lists sl ON sl.id = se.list_id \
         WHERE sl.household_id = ?1 AND se.status = ?2",
    )
    .bind(household_id)
    .bind(status)
    .fetch_one(pool)
    .await?;

    Ok(SavingsAggregate {
        label: label.to_string(),
        note: note.to_string(),
        baseline_label: BASELINE_LABEL.to_string(),
        costs_note: COSTS_NOTE.to_string(),
        cumulative_cents,
        monthly,
    })
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
