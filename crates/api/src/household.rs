//! Member-facing household data controls and member administration (issues #136
//! and #139, roadmap #114).
//!
//! The operator commands `export-household` / `delete-household` in
//! `buywise-cli` require shell access to the server host. These endpoints let a
//! signed-in household owner export and delete the household's records, and add
//! and remove its members, from the app instead, reusing the shared
//! implementations ([`buywise_core::household_data`] and
//! [`buywise_core::members`]) so the API and the CLI cannot drift.
//!
//! | Route | Method | Who | Purpose |
//! | --- | --- | --- | --- |
//! | `/api/household` | `GET` | any member | the caller's household `{ id, name, role }` |
//! | `/api/household/{id}/export` | `GET` | any member | download the documented JSON export |
//! | `/api/household/{id}/delete` | `POST` | owner | guarded delete, then end the session |
//! | `/api/household/{id}/members` | `GET` | owner | list members with their roles |
//! | `/api/household/{id}/members` | `POST` | owner | add a member by email, display name and initial password |
//! | `/api/household/{id}/members/{user_id}` | `DELETE` | owner | remove a member |
//!
//! # Authorisation
//!
//! The household id is explicit in the path and every handler requires the
//! caller to be a member of *that* household (`household_members`) — a member of
//! another household is refused with 403, never another household's data.
//! `household_members.role` (migration 003) is enforced here: export is
//! available to **every** member, while **member administration and the
//! household delete require the `owner` role** ([`require_owner`]). See
//! `docs/KNOWN_LIMITATIONS.md` for the remaining limits.
//!
//! # Guarded delete
//!
//! `POST .../delete` requires `{ "household_name", "password" }`:
//! the name must match the household's stored name exactly, and the password
//! must verify (argon2id) against the *caller's* `users.password_hash`. Either
//! mismatch is a 422 and changes nothing. On success the household's rows are
//! removed in one transaction, the caller's session row is deleted and both auth
//! cookies are cleared, so the client returns to the sign-in page.
//!
//! # Members
//!
//! `POST .../members` takes the new member's email, display name and an initial
//! password the owner shares out of band (it is never emailed and never returned
//! again); a duplicate email is a 409. `DELETE .../members/{user_id}` ends the
//! member's sessions immediately, removes their membership, and deletes their
//! account only if they belong to no other household; removing the last owner is
//! a 409. Every one of these is a mutating request, so the [`AuthenticatedUser`]
//! extractor enforces the same CSRF check as the other mutations.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::household_data::{self, HouseholdDataError};
use buywise_core::members::{self, MembershipError};

use crate::auth;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the household routes as a `Router<AppState>`, merged by
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
        .route(
            "/api/household/{household_id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/api/household/{household_id}/members/{user_id}",
            delete(remove_member),
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
    /// The caller's `household_members.role` in this household: `owner` or
    /// `member`. The app shows member administration and the delete action to
    /// owners only.
    pub role: String,
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

/// Body for `POST /api/household/{id}/members`.
#[derive(Debug, Deserialize)]
pub struct AddMemberRequest {
    /// The new member's email (`users.email`, unique across the database).
    pub email: String,
    /// The new member's display name.
    pub display_name: String,
    /// The initial password the owner shares out of band. Never returned again.
    pub password: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/household` — the caller's household id, name and their role in it.
///
/// 404 if the caller belongs to no household. Like `GET /api/settings`, this
/// assumes the single-household MVP model and picks the caller's first
/// membership deterministically when they belong to several.
async fn get_household(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<HouseholdSummary>, StatusCode> {
    let membership: Option<(String, String)> = sqlx::query_as(
        "SELECT household_id, role FROM household_members WHERE user_id = ?1 \
         ORDER BY household_id LIMIT 1",
    )
    .bind(&user.user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db_error("household.caller_membership"))?;

    let Some((household_id, role)) = membership else {
        return Err(StatusCode::NOT_FOUND);
    };

    let name = household_data::household_name(&state.pool, &household_id)
        .await
        .map_err(core_error("household.summary_name"))?;

    Ok(Json(HouseholdSummary {
        id: household_id,
        name,
        role,
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

    let document = household_data::export_household(&state.pool, &household_id, (state.clock)())
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
/// after re-entering the household name and the owner's password.
///
/// 403 unless the caller is an *owner* of `household_id` (a plain member may
/// export but never delete); 422 if the confirmation name or the password does
/// not match (nothing is changed). On success the rows are removed, the caller's
/// session is ended (row deleted, cookies cleared) and a summary is returned.
/// CSRF is enforced by [`AuthenticatedUser`] (a POST).
async fn delete_household(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Json(body): Json<DeleteHouseholdRequest>,
) -> Result<Response, StatusCode> {
    require_owner(&state, &user.user_id, &household_id).await?;

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

    let counts = household_data::delete_household(
        &state.pool,
        &household_id,
        false,
        &state.receipt_config.dir,
    )
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

/// `GET /api/household/{household_id}/members` — list the household's members,
/// with roles, for the member administration screen.
///
/// Owner-only: a plain member is refused with 403, so the member list is never
/// visible to non-owners.
async fn list_members(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
) -> Result<Json<Vec<members::Member>>, StatusCode> {
    require_owner(&state, &user.user_id, &household_id).await?;
    let member_list = members::list_members(&state.pool, &household_id)
        .await
        .map_err(membership_error("household.list_members"))?;
    Ok(Json(member_list))
}

/// `POST /api/household/{household_id}/members` — add a member with role
/// `member` by email, display name and an initial password.
///
/// Owner-only (403 otherwise); the password must pass the shared policy (422 when
/// empty); a duplicate email is 409; a vanished household is 404. The password is
/// never returned — the owner shares it with the member out of band. CSRF is
/// enforced by [`AuthenticatedUser`] (a POST).
async fn add_member(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Json(body): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<members::Member>), StatusCode> {
    require_owner(&state, &user.user_id, &household_id).await?;

    let member = members::add_member(
        &state.pool,
        &household_id,
        body.email.trim(),
        body.display_name.trim(),
        &body.password,
    )
    .await
    .map_err(membership_error("household.add_member"))?;

    Ok((StatusCode::CREATED, Json(member)))
}

/// `DELETE /api/household/{household_id}/members/{user_id}` — remove a member.
///
/// Owner-only (403 otherwise). Ends the member's sessions immediately, removes
/// their membership, and deletes their account only if they belong to no other
/// household. Refuses (409) to remove the household's last owner, and (404) a
/// user who is not a member. CSRF is enforced by [`AuthenticatedUser`] (a
/// DELETE).
async fn remove_member(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, member_user_id)): Path<(String, String)>,
) -> Result<Json<members::MemberRemoval>, StatusCode> {
    require_owner(&state, &user.user_id, &household_id).await?;

    let removal = members::remove_member(&state.pool, &household_id, &member_user_id)
        .await
        .map_err(membership_error("household.remove_member"))?;

    Ok(Json(removal))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Refuse with 403 unless `user_id` is a member of `household_id`.
///
/// A DB error is a logged 500 (fail closed), never treated as "not a member".
/// Shared with the receipt-image routes ([`crate::receipts`]), which are scoped
/// to the path's household the same way.
pub(crate) async fn require_member(
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

/// Refuse with 403 unless `user_id` is an *owner* of `household_id`.
///
/// A plain member, a member of another household and a non-member all get 403 —
/// the household's member list and delete action are never exposed to them. A DB
/// error is a logged 500 (fail closed).
async fn require_owner(
    state: &AppState,
    user_id: &str,
    household_id: &str,
) -> Result<(), StatusCode> {
    let role = members::member_role(&state.pool, household_id, user_id)
        .await
        .map_err(membership_error("household.is_owner"))?;
    match role.as_deref() {
        Some(members::ROLE_OWNER) => Ok(()),
        _ => Err(StatusCode::FORBIDDEN),
    }
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
        // Only the household-delete path can raise this, and only when a
        // receipt-image file could not be unlinked (the transaction then rolls
        // back, so nothing was deleted): a logged 500 to retry.
        HouseholdDataError::Receipts(err) => {
            tracing::error!(error = %err, "{context}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Map a membership error to its HTTP status, logging the unexpected ones.
///
/// A duplicate email or an attempt to remove the last owner is a 409 (the
/// request conflicts with the household's current state); an unacceptable
/// password is 422; an unknown household or a non-member target is 404. Those
/// cases are expected client outcomes; a hash or database failure is a logged
/// 500.
fn membership_error(context: &'static str) -> impl Fn(MembershipError) -> StatusCode {
    move |err| match &err {
        MembershipError::DuplicateEmail(_) | MembershipError::LastOwner(_) => StatusCode::CONFLICT,
        MembershipError::Password(_) | MembershipError::BlankUserField(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        MembershipError::NoSuchHousehold(_) | MembershipError::NotAMember { .. } => {
            StatusCode::NOT_FOUND
        }
        MembershipError::AlreadyBootstrapped => StatusCode::CONFLICT,
        MembershipError::Hash(detail) => {
            tracing::error!(error = %detail, "{context}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
        MembershipError::Sqlx(err) => {
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
