//! Authentication routes for the BuyWise API (P1-T2).
//!
//! Three endpoints, mounted under `/api/auth` by [`router`]:
//!
//!   * `POST /api/auth/login`  — verify the password (argon2id), create a session,
//!     and set the `session` (HttpOnly) and `csrf` (JS-readable) cookies.
//!   * `POST /api/auth/logout` — delete the current session row and clear both
//!     cookies. CSRF-protected (it is a POST behind [`AuthenticatedUser`]).
//!   * `GET  /api/auth/me`     — return the current user as JSON.
//!
//! Security invariants (see also `middleware.rs`):
//!   * Passwords are verified with argon2id via [`argon2::Argon2`]; the plaintext
//!     is never compared directly and a missing user still runs a verify against a
//!     dummy hash to keep the timing of "no such user" and "wrong password"
//!     indistinguishable.
//!   * The session cookie holds a fresh random 32-byte token; only its SHA-256 is
//!     stored (`sessions.token_hash`). The CSRF token is a separate random
//!     32-byte value stored verbatim in `sessions.csrf_token`.

use argon2::password_hash::{PasswordHash, PasswordVerifier};
use argon2::Argon2;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::middleware::{hash_token, AuthenticatedUser, CSRF_COOKIE, SESSION_COOKIE};
use crate::AppState;

/// Lifetime of a freshly minted session.
const SESSION_TTL_HOURS: i64 = 24 * 30;

/// A valid argon2id hash of a throwaway password, computed once on first use.
///
/// Verified against when no user matches the supplied email, so the "unknown
/// user" path does roughly the same work as the "wrong password" path and does
/// not leak account existence via response timing. Computed lazily (not a `const`)
/// so it is a genuine, parseable PHC string that actually exercises the KDF;
/// [`warm_dummy_hash`] forces this one-time cost at startup so even the first
/// unknown-user login only performs verification work.
fn dummy_password_hash() -> &'static str {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password_for_storage("buywise-dummy-password-for-timing")
            .expect("hashing a static dummy password cannot fail")
    })
}

/// Eagerly compute [`dummy_password_hash`] so the first unknown-user login does
/// not pay the one-time argon2 cost. Called once when the router is built (see
/// `crate::app`).
pub(crate) fn warm_dummy_hash() {
    let _ = dummy_password_hash();
}

/// Request body for `POST /api/auth/login`.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// The user's email (matched against `users.email`).
    pub email: String,
    /// The plaintext password to verify against `users.password_hash`.
    pub password: String,
}

/// JSON view of the authenticated user, returned by login and `/me`.
#[derive(Debug, Serialize)]
pub struct UserResponse {
    /// `users.id`.
    pub id: String,
    /// `users.email`.
    pub email: String,
    /// `users.display_name`.
    pub display_name: String,
}

/// Build the `/api/auth` sub-router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
}

/// `POST /api/auth/login`.
///
/// On valid credentials: mints a session, persists `SHA-256(token)` +
/// `csrf_token`, sets both cookies, and returns the user as JSON (200). On an
/// unknown email or a bad password: 401 with no cookies (and approximately
/// constant timing, see [`dummy_password_hash`]).
async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let pool = &state.pool;

    // Look up the user. A missing user must still run a verify against a dummy
    // hash so timing does not reveal whether the email exists.
    let user = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT id, email, display_name, password_hash FROM users WHERE email = ?1",
    )
    .bind(&body.email)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "login: user lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let verified_user = match user {
        Some((id, email, display_name, password_hash)) => {
            if verify_password(&body.password, &password_hash) {
                Some(UserResponse {
                    id,
                    email,
                    display_name,
                })
            } else {
                None
            }
        }
        None => {
            // Burn comparable time, ignore the (always-false) result.
            let _ = verify_password(&body.password, dummy_password_hash());
            None
        }
    };

    let user = verified_user.ok_or(StatusCode::UNAUTHORIZED)?;

    // Mint a session: random session token (cookie) + random CSRF token.
    let session_token = random_token();
    let csrf_token = random_token();
    let token_hash = hash_token(&session_token);
    let session_id = uuid::Uuid::new_v4().to_string();

    // Derive `created_at` and `expires_at` from a single instant so the two
    // timestamps are coherent.
    let issued_at = chrono::Utc::now();
    let created_at = issued_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let expires_at = (issued_at + chrono::Duration::hours(SESSION_TTL_HOURS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    // Build the response cookies *before* writing the session row: if a cookie
    // value cannot be encoded as a header, fail without leaving an orphaned
    // session row the client never received a token for.
    let mut headers = HeaderMap::new();
    append_cookie(&mut headers, &set_session_cookie(&session_token))?;
    append_cookie(&mut headers, &set_csrf_cookie(&csrf_token))?;

    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&session_id)
    .bind(&user.id)
    .bind(&token_hash)
    .bind(&csrf_token)
    .bind(&created_at)
    .bind(&expires_at)
    .execute(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "login: session insert failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((StatusCode::OK, headers, Json(user)))
}

/// `POST /api/auth/logout`.
///
/// Deletes the current session row and clears both cookies. Reaching this
/// handler already implies a valid session and a valid CSRF token (enforced by
/// the [`AuthenticatedUser`] extractor on a POST).
async fn logout(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<impl IntoResponse, StatusCode> {
    sqlx::query("DELETE FROM sessions WHERE id = ?1")
        .bind(&user.session_id)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "logout: session delete failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut headers = HeaderMap::new();
    append_cookie(&mut headers, &clear_cookie(SESSION_COOKIE, true))?;
    append_cookie(&mut headers, &clear_cookie(CSRF_COOKIE, false))?;

    Ok((StatusCode::NO_CONTENT, headers))
}

/// `GET /api/auth/me`.
///
/// Returns the current user. The [`AuthenticatedUser`] extractor already
/// rejected the request with 401 if there is no live session (GET needs no
/// CSRF token).
async fn me(user: AuthenticatedUser) -> Json<UserResponse> {
    Json(UserResponse {
        id: user.user_id,
        email: user.email,
        display_name: user.display_name,
    })
}

/// Verify a plaintext password against a stored argon2 PHC hash.
///
/// Returns `false` for any malformed stored hash or verification failure; never
/// panics and never compares plaintext directly.
fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Generate a fresh 32-byte random token as lowercase hex (64 chars).
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// `Set-Cookie` value for the session token: HttpOnly + Secure + SameSite=Strict.
fn set_session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict")
}

/// `Set-Cookie` value for the CSRF token: Secure + SameSite=Strict but NOT
/// HttpOnly, so the SPA can read it and echo it in the `X-CSRF-Token` header.
fn set_csrf_cookie(token: &str) -> String {
    format!("{CSRF_COOKIE}={token}; Path=/; Secure; SameSite=Strict")
}

/// `Set-Cookie` value that clears a cookie (empty value, Max-Age=0). Mirrors the
/// flags used when the cookie was set so browsers match and remove it.
fn clear_cookie(name: &str, http_only: bool) -> String {
    let http_only = if http_only { "; HttpOnly" } else { "" };
    format!("{name}=; Path=/; Max-Age=0{http_only}; Secure; SameSite=Strict")
}

/// Append a `Set-Cookie` header value. A cookie that cannot be encoded as a
/// header value is a server error rather than a silently dropped cookie.
fn append_cookie(headers: &mut HeaderMap, cookie: &str) -> Result<(), StatusCode> {
    let value = HeaderValue::from_str(cookie).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    headers.append(header::SET_COOKIE, value);
    Ok(())
}

/// Hash a plaintext password with argon2id, producing a PHC string suitable for
/// storage in `users.password_hash`.
///
/// The API only ever verifies passwords, never registers them; this helper
/// exists for an out-of-band operator tool that seeds `users.password_hash`
/// (tracked as a separate follow-up issue), and is used by the integration
/// tests to seed a user.
pub fn hash_password_for_storage(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use rand::rngs::OsRng;

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default().hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_is_64_hex_chars_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "tokens must be random, not constant");
    }

    #[test]
    fn hash_then_verify_round_trips() {
        let hash = hash_password_for_storage("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn verify_password_rejects_malformed_hash() {
        assert!(!verify_password("anything", "not-a-phc-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn session_cookie_flags_are_strict_and_httponly() {
        let c = set_session_cookie("tok");
        assert!(c.starts_with("session=tok"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("Secure"));
        assert!(c.contains("SameSite=Strict"));
    }

    #[test]
    fn csrf_cookie_is_readable_by_js() {
        let c = set_csrf_cookie("tok");
        assert!(c.starts_with("csrf=tok"));
        assert!(!c.contains("HttpOnly"), "csrf cookie must be JS-readable");
        assert!(c.contains("Secure"));
        assert!(c.contains("SameSite=Strict"));
    }

    #[test]
    fn clear_cookie_expires_immediately() {
        let c = clear_cookie(SESSION_COOKIE, true);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("HttpOnly"));
    }
}
