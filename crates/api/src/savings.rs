//! Savings-ledger routes for the BuyWise API (P5-T3, issue #34).
//!
//! One endpoint today: `POST /api/list/{id}/actual-spend`, which records the
//! household's actual post-shop spend against the list's `savings_events` row and
//! computes the realised `net_saving_cents`.
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
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::domain::Cents;

use crate::lists::is_member;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the savings routes as a `Router<AppState>`, merged by [`crate::app`].
///
/// The path is absolute (`/api/list/{id}/actual-spend`) so it merges alongside
/// the other `/api/list/{id}` shapes in `lists::router` rather than nesting.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/list/{id}/actual-spend", post(record_actual_spend))
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

/// Map a DB error to a logged 500, so a transient failure never fails open.
/// Mirrors `lists::db_error`.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "savings: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}
