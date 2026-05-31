//! Authentication and CSRF middleware for the BuyWise API (P1-T2).
//!
//! The session model (see `migrations/003_phase1_schema.sql`):
//!
//!   * A login mints a random 32-byte **session token**. The raw token is the
//!     value of the `session` cookie and is NEVER stored; only its SHA-256 hex
//!     digest is persisted as `sessions.token_hash`.
//!   * A separate random 32-byte **CSRF token** is stored verbatim in
//!     `sessions.csrf_token` and returned in a non-HttpOnly `csrf` cookie so the
//!     SPA can echo it back in the `X-CSRF-Token` header.
//!   * Every state-changing request (POST/PUT/PATCH/DELETE) must present an
//!     `X-CSRF-Token` header that equals `sessions.csrf_token`, compared in
//!     constant time. A missing or mismatched header is rejected with 403.
//!
//! [`AuthenticatedUser`] is an Axum [`FromRequestParts`] extractor that resolves
//! the `session` cookie to a live (non-expired) session + user, and — for unsafe
//! methods — enforces the CSRF check. Any failure short-circuits with the correct
//! status (401/403); there is no default-allow path.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, Method, StatusCode};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::AppState;

/// The cookie carrying the raw session token (HttpOnly).
pub const SESSION_COOKIE: &str = "session";
/// The cookie carrying the CSRF token (readable by JS; not HttpOnly).
pub const CSRF_COOKIE: &str = "csrf";
/// Header the client echoes the CSRF token back in on mutating requests.
pub const CSRF_HEADER: &str = "X-CSRF-Token";

/// SHA-256 hex digest of a session token. The stored `sessions.token_hash` is
/// always this digest of the raw cookie value, so a database leak never exposes
/// a usable bearer token.
pub fn hash_token(raw_token: &str) -> String {
    let digest = Sha256::digest(raw_token.as_bytes());
    hex::encode(digest)
}

/// Constant-time byte comparison, used for the CSRF token check so a timing side
/// channel cannot reveal how many leading bytes matched.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A session row resolved from a live (non-expired) `session` cookie, together
/// with the authenticated user's identity.
///
/// Used both as a request guard (its presence proves an authenticated, CSRF-valid
/// request) and as a data carrier for handlers that need the current user.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    /// `sessions.id` — the opaque session id (NOT the raw cookie token).
    pub session_id: String,
    /// `users.id`.
    pub user_id: String,
    /// `users.email`.
    pub email: String,
    /// `users.display_name`.
    pub display_name: String,
    /// `sessions.csrf_token` for this session.
    pub csrf_token: String,
}

/// One joined `sessions` + `users` row for a live session, as read by
/// [`lookup_session`]. The `expires_at` is compared against now in Rust so the
/// query is index-friendly on `token_hash`.
struct SessionRow {
    session_id: String,
    user_id: String,
    email: String,
    display_name: String,
    csrf_token: String,
    expires_at: String,
}

/// Look up a session by the SHA-256 of its raw token, joined to its user.
///
/// Returns the row only if a session with that `token_hash` exists; expiry is
/// evaluated by the caller so a clear distinction between "no such session" and
/// "expired" is possible if ever needed. A DB error is surfaced (never silently
/// treated as "no session") so a transient failure cannot fail open.
async fn lookup_session(
    pool: &SqlitePool,
    token_hash: &str,
) -> Result<Option<SessionRow>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, String, String, String, String, String)>(
        "SELECT s.id, s.user_id, u.email, u.display_name, s.csrf_token, s.expires_at \
         FROM sessions s \
         JOIN users u ON u.id = s.user_id \
         WHERE s.token_hash = ?1",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(session_id, user_id, email, display_name, csrf_token, expires_at)| SessionRow {
            session_id,
            user_id,
            email,
            display_name,
            csrf_token,
            expires_at,
        },
    ))
}

/// Extract the value of a named cookie from the request's `Cookie` header.
///
/// Parses the standard `name=value; name2=value2` cookie syntax. Returns the
/// first matching cookie's value (trimmed of surrounding whitespace), or `None`.
pub fn cookie_value(parts: &Parts, name: &str) -> Option<String> {
    let header = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

impl FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 1. A session cookie must be present.
        let raw_token = cookie_value(parts, SESSION_COOKIE).ok_or(StatusCode::UNAUTHORIZED)?;

        // 2. Resolve it to a live session. A DB failure is a 500, not a silent
        //    allow; a missing row or an expired session is a 401.
        let token_hash = hash_token(&raw_token);
        let session = lookup_session(&state.pool, &token_hash)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::UNAUTHORIZED)?;

        if !session_is_live(&session.expires_at) {
            return Err(StatusCode::UNAUTHORIZED);
        }

        // 3. For state-changing methods, enforce CSRF: the X-CSRF-Token header
        //    must equal the session's csrf_token (constant-time compare).
        if is_mutating_method(&parts.method) {
            let header_token = parts
                .headers
                .get(CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::FORBIDDEN)?;

            if !constant_time_eq(header_token.as_bytes(), session.csrf_token.as_bytes()) {
                return Err(StatusCode::FORBIDDEN);
            }
        }

        Ok(AuthenticatedUser {
            session_id: session.session_id,
            user_id: session.user_id,
            email: session.email,
            display_name: session.display_name,
            csrf_token: session.csrf_token,
        })
    }
}

/// True for HTTP methods that mutate state and therefore require a CSRF token.
/// GET/HEAD/OPTIONS/TRACE are safe and exempt.
fn is_mutating_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// True if an ISO-8601 `expires_at` is strictly in the future relative to now.
///
/// Timestamps are stored as RFC-3339 strings (see `db::now_iso`). An unparseable
/// timestamp is treated as expired (fail closed) rather than as still-valid.
fn session_is_live(expires_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(expires_at) {
        Ok(expiry) => expiry > chrono::Utc::now(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_token_is_sha256_hex_of_input() {
        // SHA-256("") well-known digest.
        assert_eq!(
            hash_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // Distinct inputs hash differently and are 64 hex chars.
        let h = hash_token("some-random-token");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, hash_token("some-random-tokeo"));
    }

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn session_is_live_respects_expiry() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let past = (chrono::Utc::now() - chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        assert!(session_is_live(&future));
        assert!(!session_is_live(&past));
        assert!(!session_is_live("not-a-timestamp"));
    }

    #[test]
    fn mutating_methods_require_csrf() {
        assert!(is_mutating_method(&Method::POST));
        assert!(is_mutating_method(&Method::PUT));
        assert!(is_mutating_method(&Method::PATCH));
        assert!(is_mutating_method(&Method::DELETE));
        assert!(!is_mutating_method(&Method::GET));
        assert!(!is_mutating_method(&Method::HEAD));
        assert!(!is_mutating_method(&Method::OPTIONS));
    }
}
