//! Household settings routes (`GET`/`PATCH /api/settings`) — P4-T2 + issue #98.
//!
//! Exposes the two configurable convenience thresholds (cents) the basket
//! optimiser consults when a `POST /api/optimise` request omits an explicit
//! threshold (see [`crate::optimise`]), and the per-retailer store settings
//! (`active` / `occasional` / `off`, plus a `nearby` flag) that decide which
//! stores take part in plans (issue #98). Both endpoints are household-scoped to
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
use buywise_core::domain::{Cents, Retailer, StoreStatus};
use buywise_core::optimiser::{StoreSetting, DEFAULT_THRESHOLDS};

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
/// effective convenience thresholds in cents, plus its per-retailer store
/// settings (issue #98).
#[derive(Debug, Serialize)]
pub struct SettingsResponse {
    /// Second-store convenience threshold (cents). Default €10.00 (1000).
    pub convenience_threshold_2_cents: Cents,
    /// Third-store convenience threshold (cents). Default €25.00 (2500).
    pub convenience_threshold_3_cents: Cents,
    /// One entry per retailer ([`Retailer::ALL`], always all five), each carrying
    /// its status (`active` / `occasional` / `off`) and `nearby` flag. A retailer
    /// with no stored row is returned as `active`, not nearby.
    pub stores: Vec<StoreSetting>,
}

/// Body for `PATCH /api/settings`.
///
/// All fields are optional so a client can update one thing without restating the
/// rest; an absent field leaves the stored value unchanged. At least one field
/// must be present (an empty body is a no-op 422).
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
    /// Per-store updates. Each entry names a retailer and must set at least one of
    /// `status`/`nearby`; an absent field keeps its stored (or default) value.
    /// Defaults to empty when omitted, so a thresholds-only patch is unchanged.
    #[serde(default)]
    pub stores: Vec<StoreSettingsPatch>,
}

/// One store's update within [`PatchSettingsRequest::stores`] (issue #98).
#[derive(Debug, Deserialize)]
pub struct StoreSettingsPatch {
    /// The retailer this update applies to. An unknown id is a serde error and so
    /// rejected as 422 by the `Json` extractor.
    pub retailer: Retailer,
    /// New status when present.
    pub status: Option<StoreStatus>,
    /// New nearby flag when present.
    pub nearby: Option<bool>,
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

/// A household's stored per-retailer settings (issue #98), in `retailer`
/// canonical order. Retailers with no stored row are simply absent; callers
/// treat them as [`StoreStatus::Active`] and not nearby (see
/// [`full_store_settings`]).
///
/// Public so [`crate::optimise`] can build the optimiser input's store settings
/// from the same source the settings endpoint writes.
pub async fn stored_store_settings(
    pool: &sqlx::SqlitePool,
    household_id: &str,
) -> Result<Vec<StoreSetting>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (Retailer, StoreStatus, i64)>(
        "SELECT retailer, status, nearby FROM household_store_settings \
         WHERE household_id = ?1 ORDER BY retailer",
    )
    .bind(household_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(retailer, status, nearby)| StoreSetting {
            retailer,
            status,
            nearby: nearby != 0,
        })
        .collect())
}

/// Expand stored settings to the full [`Retailer::ALL`] list, filling any absent
/// retailer with the default `active` / not-nearby so the settings response always
/// shows all five stores.
fn full_store_settings(stored: &[StoreSetting]) -> Vec<StoreSetting> {
    Retailer::ALL
        .iter()
        .map(|&retailer| {
            stored
                .iter()
                .find(|s| s.retailer == retailer)
                .cloned()
                .unwrap_or(StoreSetting {
                    retailer,
                    status: StoreStatus::Active,
                    nearby: false,
                })
        })
        .collect()
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

    let stores = full_store_settings(
        &stored_store_settings(pool, &household_id)
            .await
            .map_err(db_error("get_settings.stores"))?,
    );

    Ok(Json(SettingsResponse {
        convenience_threshold_2_cents: threshold_2,
        convenience_threshold_3_cents: threshold_3,
        stores,
    }))
}

/// `PATCH /api/settings` — update the convenience thresholds and/or per-store
/// settings.
///
/// 404 if the caller belongs to no household; 422 if the body changes nothing, a
/// store entry sets neither `status` nor `nearby`, or the effective
/// `threshold_3 < threshold_2` (validated post-merge, consistent with
/// `optimise.rs`: a partial update must not leave the pair out of order). CSRF is
/// enforced by the extractor (this is a PATCH). Rows are upserted so a household
/// lacking one still gets it; `updated_at` is refreshed.
async fn patch_settings(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<PatchSettingsRequest>,
) -> Result<Json<SettingsResponse>, StatusCode> {
    let pool = &state.pool;

    // Validate the body shape before any state lookup so a no-op request is a
    // clean 422 regardless of authorization (mirrors patch_list ordering). A
    // store entry must set at least one of its fields — naming a retailer alone
    // changes nothing.
    let no_thresholds = body.convenience_threshold_2_cents.is_none()
        && body.convenience_threshold_3_cents.is_none();
    if (no_thresholds && body.stores.is_empty())
        || body
            .stores
            .iter()
            .any(|s| s.status.is_none() && s.nearby.is_none())
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

    // Validate the *effective* (post-merge) pair, not just the provided fields,
    // so updating one side cannot leave a negative or out-of-order value stored.
    // Thresholds are cents, so they must be non-negative (a negative threshold
    // would let an equally-complete, MORE-expensive split clear `saving >
    // threshold` in the core), and a third-store threshold can never be easier to
    // justify than a second. `threshold_2 >= 0` plus the ordering guard implies
    // `threshold_3 >= 0`.
    if threshold_2 < 0 || threshold_3 < threshold_2 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Merge each store update onto the household's effective setting (stored row,
    // or the active/not-nearby default) so an omitted field keeps its value.
    let mut merged = full_store_settings(
        &stored_store_settings(pool, &household_id)
            .await
            .map_err(db_error("patch_settings.stores_load"))?,
    );
    for patch in &body.stores {
        let entry = merged
            .iter_mut()
            .find(|s| s.retailer == patch.retailer)
            .expect("full_store_settings covers every Retailer::ALL entry");
        if let Some(status) = patch.status {
            entry.status = status;
        }
        if let Some(nearby) = patch.nearby {
            entry.nearby = nearby;
        }
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

    // Write the touched store rows in one transaction so a partial failure cannot
    // leave some stores updated and others not.
    if !body.stores.is_empty() {
        let mut tx = pool
            .begin()
            .await
            .map_err(db_error("patch_settings.stores_tx"))?;
        for patch in &body.stores {
            let entry = merged
                .iter()
                .find(|s| s.retailer == patch.retailer)
                .expect("merged above");
            sqlx::query(
                "INSERT INTO household_store_settings \
                     (household_id, retailer, status, nearby, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(household_id, retailer) DO UPDATE SET \
                     status = excluded.status, \
                     nearby = excluded.nearby, \
                     updated_at = excluded.updated_at",
            )
            .bind(&household_id)
            .bind(entry.retailer)
            .bind(entry.status)
            .bind(if entry.nearby { 1_i64 } else { 0_i64 })
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(db_error("patch_settings.stores_upsert"))?;
        }
        tx.commit()
            .await
            .map_err(db_error("patch_settings.stores_commit"))?;
    }

    Ok(Json(SettingsResponse {
        convenience_threshold_2_cents: threshold_2,
        convenience_threshold_3_cents: threshold_3,
        stores: merged,
    }))
}
