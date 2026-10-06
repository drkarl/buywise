//! Member-facing household data controls (issue #136, roadmap #114).
//!
//! The operator commands `export-household` / `delete-household` in
//! `buywise-cli` require shell access to the server host. These endpoints let a
//! signed-in household member export and delete the household's records from the
//! app instead, reusing the same shared implementation
//! ([`buywise_core::household_data`]) so the API and the CLI cannot drift.
//!
//! | Route | Method | Purpose |
//! | --- | --- | --- |
//! | `/api/household` | `GET` | the caller's household `{ id, name }` |
//! | `/api/household/{household_id}/export` | `GET` | download the documented JSON export |
//! | `/api/household/{household_id}/delete` | `POST` | guarded delete, then end the session |
//!
//! # Authorisation
//!
//! The household id is explicit in the path and every handler requires the
//! caller to be a member of *that* household (`household_members`, via
//! [`crate::lists::is_member`]) — a member of another household is refused with
//! 403, never another household's data. There is no owner/admin concept in the
//! code (migration 003's `household_members.role` is stored but unenforced), so
//! any member may export, and any member may delete after re-entering their own
//! password and typing the household name; see `docs/KNOWN_LIMITATIONS.md`.
//!
//! # Guarded delete
//!
//! `POST .../delete` requires `{ "household_name", "password" }`:
//! the name must match the household's stored name exactly, and the password
//! must verify (argon2id) against the *caller's* `users.password_hash`. Either
//! mismatch is a 422 and changes nothing. On success the household's rows are
//! removed in one transaction, the caller's session row is deleted and both auth
//! cookies are cleared, so the client returns to the sign-in page.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::household_data::{self, HouseholdDataError};

use crate::auth;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the household data-controls routes as a `Router<AppState>`, merged by
/// [`crate::api_router`]. Paths are absolute, like the sibling modules.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/household", get(get_household))
        .route(
            "/api/household/{household_id}/export",
            get(export_household),
        )
        .route(
            "/api/household/{household_id}/delete",
            post(delete_household),
        )
}

// ---------------------------------------------------------------------------
// Response / request shapes
// ---------------------------------------------------------------------------

/// `GET /api/household` response: the caller's household.
#[derive(Debug, Serialize)]
pub struct HouseholdSummary {
    /// `households.id`.
    pub id: String,
    /// `households.name` — the exact string the delete confirmation must repeat.
    pub name: String,
}

/// Body for `POST /api/household/{id}/delete`.
#[derive(Debug, Deserialize)]
pub struct DeleteHouseholdRequest {
    /// Must equal the household's stored name, typed by the member.
    pub household_name: String,
    /// The caller's own password, re-entered to authorise the delete.
    pub password: String,
}

/// `POST /api/household/{id}/delete` success body.
#[derive(Debug, Serialize)]
pub struct DeleteHouseholdResponse {
    /// Total household-scoped rows removed across every table.
    pub removed_rows: u64,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/household` — the caller's household id and name.
///
/// 404 if the caller belongs to no household. Like `GET /api/settings`, this
/// assumes the single-household MVP model and picks the caller's first
/// membership deterministically when they belong to several.
async fn get_household(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<HouseholdSummary>, StatusCode> {
    let household_id: Option<String> = sqlx::query_scalar(
        "SELECT household_id FROM household_members WHERE user_id = ?1 \
         ORDER BY household_id LIMIT 1",
    )
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error("household.caller_membership"))?;

    let Some(household_id) = household_id else {
        return Err(StatusCode::NOT_FOUND);
    };

    let name = household_data::household_name(&state.pool, &household_id)
        .await
        .map_err(core_error("household.summary_name"))?;

    Ok(Json(HouseholdSummary {
        id: household_id,
        name,
    }))
}

/// `GET /api/household/{household_id}/export` — download the documented JSON
/// export ([`docs/household-export.md`](../../docs/household-export.md)).
///
/// 403 unless the caller is a member of `household_id`. The body is exactly the
/// document `buywise-cli export-household` writes, served as an attachment so a
/// browser saves it rather than rendering it.
async fn export_household(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
) -> Result<Response, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;

    let document = household_data::export_household(&state.pool, &household_id)
        .await
        .map_err(core_error("household.export"))?;

    let filename = format!("buywise-household-{}.json", safe_id(&household_id));
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|e| {
            tracing::error!(error = %e, "household.export: invalid content-disposition");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut response = Json(document).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, disposition);
    Ok(response)
}

/// `POST /api/household/{household_id}/delete` — delete the household's records
/// after re-entering the household name and the member's password.
///
/// 403 unless the caller is a member of `household_id`; 422 if the confirmation
/// name or the password does not match (nothing is changed). On success the rows
/// are removed, the caller's session is ended (row deleted, cookies cleared) and
/// a summary is returned. CSRF is enforced by [`AuthenticatedUser`] (a POST).
async fn delete_household(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Json(body): Json<DeleteHouseholdRequest>,
) -> Result<Response, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;

    // The typed name must match the stored name exactly; look the name up first
    // so a household deleted a moment ago is a 404, not a name mismatch.
    let expected_name = household_data::household_name(&state.pool, &household_id)
        .await
        .map_err(core_error("household.delete_name"))?;
    if body.household_name != expected_name {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Re-verify the caller's password before anything is removed.
    let stored_hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?1")
            .bind(&user.user_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_error("household.delete_password"))?;
    let Some(stored_hash) = stored_hash else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !buywise_core::password::verify_password(&body.password, &stored_hash) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let counts = household_data::delete_household(&state.pool, &household_id, false)
        .await
        .map_err(core_error("household.delete"))?;
    let removed_rows: u64 = counts.iter().map(|(_, count)| count).sum();

    // End the caller's session. `delete_household` already removed the session
    // rows of members left with no other household; this covers a member who
    // also belongs to another household. The deletion itself has succeeded, so a
    // session-cleanup failure is logged rather than failing the request.
    if let Err(err) = sqlx::query("DELETE FROM sessions WHERE id = ?1")
        .bind(&user.session_id)
        .execute(&state.pool)
        .await
    {
        tracing::error!(error = %err, "household.delete: session cleanup failed");
    }

    let mut headers = HeaderMap::new();
    auth::clear_auth_cookies(&mut headers)?;

    Ok((
        StatusCode::OK,
        headers,
        Json(DeleteHouseholdResponse { removed_rows }),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Refuse with 403 unless `user_id` is a member of `household_id`.
///
/// A DB error is a logged 500 (fail closed), never treated as "not a member".
async fn require_member(
    state: &AppState,
    user_id: &str,
    household_id: &str,
) -> Result<(), StatusCode> {
    crate::lists::is_member(&state.pool, user_id, household_id)
        .await
        .map_err(db_error("household.is_member"))
        .and_then(|member| {
            if member {
                Ok(())
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        })
}

/// Map a household-data error: a vanished household is 404, anything else is a
/// logged 500.
fn core_error(context: &'static str) -> impl Fn(HouseholdDataError) -> StatusCode {
    move |err| match err {
        HouseholdDataError::NoSuchHousehold(_) => StatusCode::NOT_FOUND,
        HouseholdDataError::Sqlx(err) => {
            tracing::error!(error = %err, "{context}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Map a DB error to a logged 500, so a transient failure never fails open.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |err| {
        tracing::error!(error = %err, "{context}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// Restrict a household id to characters safe in a `Content-Disposition`
/// filename. The id has already passed a membership check, so this only guards
/// against unusual seeded ids.
fn safe_id(household_id: &str) -> String {
    household_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}
