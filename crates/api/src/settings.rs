//! Household settings routes (`GET`/`PATCH /api/settings`) — P4-T2.
//!
//! Exposes the two configurable convenience thresholds (cents) the basket
//! optimiser consults when a `POST /api/optimise` request omits an explicit
//! threshold (see [`crate::optimise`]). Both endpoints are household-scoped to
//! the authenticated caller: there is no `household_id` path/query parameter —
//! the household is derived from the caller's `household_members` membership,
//! exactly as the collection list endpoints do.
//!
//! # Single-household assumption (known limitation)
//!
//! These endpoints assume the caller belongs to exactly one household. With no
//! `household_id` parameter in the P4-T2 contract, [`caller_household`] picks the
//! caller's first membership (`ORDER BY household_id LIMIT 1`). `POST
//! /api/optimise`, by contrast, resolves the household from the *list* being
//! optimised. So for a user in more than one household these two can diverge: a
//! `PATCH /api/settings` could update household A's thresholds while an optimise
//! run on a household-B list reads B's stored values. This is an accepted
//! limitation of the parameter-less contract, not a bug — the MVP household model
//! is single-household. Adding an explicit `household_id` parameter (and the
//! membership check it implies) is the forward fix when multi-household lands.
//!
//! Storage: the `household_settings` table (migration 005) holds at most one row
//! per household, back-filled with the documented defaults (€10 / €25) for every
//! household at migration time. A row therefore normally always exists; `GET`
//! defensively falls back to those same defaults if a row is somehow absent (a
//! household created after the back-fill without its own settings row), so the
//! endpoint never 404s on a household the caller belongs to.
//!
//! Error discipline mirrors the sibling handlers (`lists.rs`, `optimise.rs`): a
//! database error is logged and surfaced as 500 (never failing open); 401 without
//! a live session and 403 for a missing/invalid CSRF token are enforced by the
//! [`AuthenticatedUser`] extractor; a `PATCH` whose effective
//! `threshold_3 < threshold_2` is 422 (consistent with `optimise.rs`); a caller
//! who belongs to no household is 404.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::db;
use buywise_core::domain::Cents;
use buywise_core::optimiser::DEFAULT_THRESHOLDS;

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the settings routes as a `Router<AppState>`, merged by [`crate::app`].
///
/// Both verbs share the one `/api/settings` path: `GET` reads the household's
/// current thresholds, `PATCH` updates them (CSRF-protected by the extractor).
pub fn router() -> Router<AppState> {
    Router::new().route("/api/settings", get(get_settings).patch(patch_settings))
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// Response for `GET /api/settings` and `PATCH /api/settings`: the household's
/// effective convenience thresholds in cents.
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    /// Second-store convenience threshold (cents). Default €10.00 (1000).
    pub convenience_threshold_2_cents: Cents,
    /// Third-store convenience threshold (cents). Default €25.00 (2500).
    pub convenience_threshold_3_cents: Cents,
}

/// Body for `PATCH /api/settings`.
///
/// Both fields are optional so a client can update one threshold without
/// restating the other; an absent field leaves the stored value unchanged. At
/// least one field must be present (an empty body is a no-op 422).
///
/// An explicit JSON `null` is treated identically to an omitted field (serde maps
/// both to `None` for an `Option`), i.e. "leave unchanged" — there is no
/// "clear to default" semantic, since the thresholds are non-nullable in storage.
#[derive(Debug, Deserialize)]
pub struct PatchSettingsRequest {
    /// New second-store threshold (cents) when present.
    pub convenience_threshold_2_cents: Option<Cents>,
    /// New third-store threshold (cents) when present.
    pub convenience_threshold_3_cents: Option<Cents>,
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by optimise.rs)
// ---------------------------------------------------------------------------

/// A household's stored convenience thresholds, or `None` if no row exists.
///
/// Public so [`crate::optimise`] can fall back to the household's stored values
/// when a request omits a threshold (AC #3). The defaults baked into migration
/// 005 mean a row normally always exists; a `None` here means the household has
/// no settings row at all, and callers fall back to the core constants.
pub async fn stored_thresholds(
    pool: &sqlx::SqlitePool,
    household_id: &str,
) -> Result<Option<(Cents, Cents)>, sqlx::Error> {
    sqlx::query_as::<_, (Cents, Cents)>(
        "SELECT convenience_threshold_2_cents, convenience_threshold_3_cents \
         FROM household_settings WHERE household_id = ?1",
    )
    .bind(household_id)
    .fetch_optional(pool)
    .await
}

// ---------------------------------------------------------------------------
// Authorization helper
// ---------------------------------------------------------------------------

/// The (single) household the caller belongs to, or `None` if they belong to no
/// household.
///
/// Mirrors the `(SELECT household_id FROM household_members WHERE user_id = ?1)`
/// sub-select the list endpoints use. A user may belong to several households;
/// the settings endpoints have no `household_id` parameter, so — like the list
/// collection endpoints — they operate on the caller's membership directly. The
/// `ORDER BY` makes the choice deterministic when a user is in more than one.
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
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "settings: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/settings` — the caller's household convenience thresholds.
///
/// 404 if the caller belongs to no household. Returns the stored row, or the
/// documented defaults (€10 / €25) if no settings row exists for the household.
async fn get_settings(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<SettingsResponse>, StatusCode> {
    let pool = &state.pool;

    let household_id = caller_household(pool, &user.user_id)
        .await
        .map_err(db_error("get_settings.household"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    let (threshold_2, threshold_3) = stored_thresholds(pool, &household_id)
        .await
        .map_err(db_error("get_settings.load"))?
        .unwrap_or(DEFAULT_THRESHOLDS);

    Ok(Json(SettingsResponse {
        convenience_threshold_2_cents: threshold_2,
        convenience_threshold_3_cents: threshold_3,
    }))
}

/// `PATCH /api/settings` — update one or both convenience thresholds.
///
/// 404 if the caller belongs to no household; 422 if the body changes nothing or
/// the effective `threshold_3 < threshold_2` (validated post-merge, consistent
/// with `optimise.rs`: a partial update must not leave the pair out of order).
/// CSRF is enforced by the extractor (this is a PATCH). The row is upserted so a
/// household lacking a settings row still gets one; `updated_at` is refreshed.
async fn patch_settings(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<PatchSettingsRequest>,
) -> Result<Json<SettingsResponse>, StatusCode> {
    let pool = &state.pool;

    // Validate the body shape before any state lookup so a no-op request is a
    // clean 422 regardless of authorization (mirrors patch_list ordering).
    if body.convenience_threshold_2_cents.is_none() && body.convenience_threshold_3_cents.is_none()
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let household_id = caller_household(pool, &user.user_id)
        .await
        .map_err(db_error("patch_settings.household"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Merge the partial update onto the current effective values (stored row, or
    // the documented defaults if none) so an omitted field keeps its old value.
    let (current_2, current_3) = stored_thresholds(pool, &household_id)
        .await
        .map_err(db_error("patch_settings.load"))?
        .unwrap_or(DEFAULT_THRESHOLDS);

    let threshold_2 = body.convenience_threshold_2_cents.unwrap_or(current_2);
    let threshold_3 = body.convenience_threshold_3_cents.unwrap_or(current_3);

    // A third-store threshold can never be easier to justify than a second.
    // Validate the *effective* (post-merge) pair, not just the provided fields,
    // so updating one side cannot leave the stored pair out of order.
    if threshold_3 < threshold_2 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let now = db::now_iso();
    sqlx::query(
        "INSERT INTO household_settings \
             (household_id, convenience_threshold_2_cents, convenience_threshold_3_cents, updated_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(household_id) DO UPDATE SET \
             convenience_threshold_2_cents = excluded.convenience_threshold_2_cents, \
             convenience_threshold_3_cents = excluded.convenience_threshold_3_cents, \
             updated_at = excluded.updated_at",
    )
    .bind(&household_id)
    .bind(threshold_2)
    .bind(threshold_3)
    .bind(&now)
    .execute(pool)
    .await
    .map_err(db_error("patch_settings.upsert"))?;

    Ok(Json(SettingsResponse {
        convenience_threshold_2_cents: threshold_2,
        convenience_threshold_3_cents: threshold_3,
    }))
}
