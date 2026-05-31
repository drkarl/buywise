//! Integration tests for the P1-T2 auth endpoints (issue #13).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied, exercising every acceptance
//! criterion and the three testing-expectation scenarios end-to-end:
//!   * login (good/bad password) → cookies / 401
//!   * /me with and without a session
//!   * CSRF missing / wrong on a POST → 403
//!   * logout clears cookies + deletes the session row, after which /me is 401
//!   * an expired session is rejected with 401
//!   * `sessions.token_hash` is the SHA-256 of the cookie value, not the raw token

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::middleware::hash_token;
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const TEST_EMAIL: &str = "alice@example.test";
const TEST_PASSWORD: &str = "correct horse battery staple";

/// Build a fresh in-memory pool with migrations + one user whose password is
/// `TEST_PASSWORD` (argon2id-hashed via the production hashing path).
async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");

    let hash =
        buywise_api::auth::hash_password_for_storage(TEST_PASSWORD).expect("hash test password");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind("user-alice")
    .bind(TEST_EMAIL)
    .bind(&hash)
    .bind("Alice")
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert user");

    pool
}

/// Extract the value of a named cookie from all Set-Cookie headers on a response.
fn cookie_from_set_cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    for value in resp.headers().get_all(header::SET_COOKIE) {
        let s = value.to_str().ok()?;
        let first = s.split(';').next()?;
        if let Some((k, v)) = first.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Returns true if any Set-Cookie for `name` is an immediate-expiry clear.
fn cookie_is_cleared(resp: &axum::response::Response, name: &str) -> bool {
    for value in resp.headers().get_all(header::SET_COOKIE) {
        let s = value.to_str().unwrap_or("");
        if s.starts_with(&format!("{name}=")) && s.contains("Max-Age=0") {
            return true;
        }
    }
    false
}

fn login_request(email: &str, password: &str) -> Request<Body> {
    let body = serde_json::json!({ "email": email, "password": password }).to_string();
    Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn login_with_correct_credentials_sets_both_cookies() {
    let pool = setup_pool().await;
    let state = AppState { pool };
    let resp = app(state)
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let session = cookie_from_set_cookie(&resp, "session").expect("session cookie");
    let csrf = cookie_from_set_cookie(&resp, "csrf").expect("csrf cookie");
    assert_eq!(session.len(), 64, "session token is 32-byte hex");
    assert_eq!(csrf.len(), 64, "csrf token is 32-byte hex");

    // The HttpOnly/Secure/SameSite flags are exactly as specified.
    let session_header = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .find(|s| s.starts_with("session="))
        .unwrap();
    assert!(session_header.contains("HttpOnly"));
    assert!(session_header.contains("Secure"));
    assert!(session_header.contains("SameSite=Strict"));

    let csrf_header = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .find(|s| s.starts_with("csrf="))
        .unwrap();
    assert!(
        !csrf_header.contains("HttpOnly"),
        "csrf must be JS-readable"
    );
    assert!(csrf_header.contains("Secure"));
    assert!(csrf_header.contains("SameSite=Strict"));
}

#[tokio::test]
async fn login_with_wrong_password_is_401_and_sets_no_cookies() {
    let pool = setup_pool().await;
    let state = AppState { pool };
    let resp = app(state)
        .oneshot(login_request(TEST_EMAIL, "wrong-password"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(cookie_from_set_cookie(&resp, "session").is_none());
    assert!(cookie_from_set_cookie(&resp, "csrf").is_none());
}

#[tokio::test]
async fn login_with_unknown_user_is_401() {
    let pool = setup_pool().await;
    let state = AppState { pool };
    let resp = app(state)
        .oneshot(login_request("nobody@example.test", TEST_PASSWORD))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stored_token_hash_is_sha256_of_cookie_value() {
    let pool = setup_pool().await;
    let state = AppState { pool: pool.clone() };
    let resp = app(state)
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    let session_token = cookie_from_set_cookie(&resp, "session").expect("session cookie");

    // The raw token must NOT be stored; only its SHA-256 hex.
    let raw_present: Option<String> =
        sqlx::query_scalar("SELECT id FROM sessions WHERE token_hash = ?1")
            .bind(&session_token)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(raw_present.is_none(), "raw token must never be stored");

    let expected_hash = hex::encode(Sha256::digest(session_token.as_bytes()));
    assert_eq!(expected_hash, hash_token(&session_token));
    let hashed_present: Option<String> =
        sqlx::query_scalar("SELECT id FROM sessions WHERE token_hash = ?1")
            .bind(&expected_hash)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert!(hashed_present.is_some(), "SHA-256(token) must be stored");
}

#[tokio::test]
async fn me_with_valid_session_returns_user_and_without_is_401() {
    let pool = setup_pool().await;

    // No cookie → 401.
    let resp = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Login, then /me with the session cookie → 200 + user JSON.
    let login = app(AppState { pool: pool.clone() })
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    let session = cookie_from_set_cookie(&login, "session").unwrap();

    let resp = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header(header::COOKIE, format!("session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["email"], TEST_EMAIL);
    assert_eq!(json["display_name"], "Alice");
}

#[tokio::test]
async fn logout_clears_cookies_deletes_session_and_me_then_401() {
    let pool = setup_pool().await;

    let login = app(AppState { pool: pool.clone() })
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    let session = cookie_from_set_cookie(&login, "session").unwrap();
    let csrf = cookie_from_set_cookie(&login, "csrf").unwrap();

    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 1);

    let logout = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header(header::COOKIE, format!("session={session}"))
                .header("X-CSRF-Token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    assert!(cookie_is_cleared(&logout, "session"));
    assert!(cookie_is_cleared(&logout, "csrf"));

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 0, "logout must delete the session row");

    // /me with the now-deleted session → 401.
    let resp = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header(header::COOKIE, format!("session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn post_without_csrf_header_is_403() {
    let pool = setup_pool().await;
    let login = app(AppState { pool: pool.clone() })
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    let session = cookie_from_set_cookie(&login, "session").unwrap();

    // POST /logout with a valid session but no X-CSRF-Token → 403.
    let resp = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header(header::COOKIE, format!("session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn post_with_wrong_csrf_header_is_403() {
    let pool = setup_pool().await;
    let login = app(AppState { pool: pool.clone() })
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    let session = cookie_from_set_cookie(&login, "session").unwrap();

    let resp = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header(header::COOKIE, format!("session={session}"))
                .header("X-CSRF-Token", "not-the-real-csrf-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn expired_session_is_rejected_with_401() {
    let pool = setup_pool().await;

    // Insert a session whose expires_at is in the past.
    let raw_token = "deadbeef".repeat(8); // 64 hex chars
    let token_hash = hash_token(&raw_token);
    let past = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind("expired-session")
    .bind("user-alice")
    .bind(&token_hash)
    .bind("some-csrf")
    .bind(db::now_iso())
    .bind(&past)
    .execute(&pool)
    .await
    .unwrap();

    let resp = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header(header::COOKIE, format!("session={raw_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_session_cookie_is_rejected_with_401() {
    let pool = setup_pool().await;

    // A syntactically valid token whose hash matches no stored session.
    let resp = app(AppState { pool })
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header(header::COOKIE, format!("session={}", "f00d".repeat(16)))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn second_logout_after_session_deleted_is_401() {
    let pool = setup_pool().await;

    let login = app(AppState { pool: pool.clone() })
        .oneshot(login_request(TEST_EMAIL, TEST_PASSWORD))
        .await
        .unwrap();
    let session = cookie_from_set_cookie(&login, "session").unwrap();
    let csrf = cookie_from_set_cookie(&login, "csrf").unwrap();

    // First logout deletes the row and returns 204.
    let first = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header(header::COOKIE, format!("session={session}"))
                .header("X-CSRF-Token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT);

    // Second logout with the same (now-deleted) session is rejected at the
    // extractor with 401, before the DELETE runs — logout is idempotent.
    let second = app(AppState { pool: pool.clone() })
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header(header::COOKIE, format!("session={session}"))
                .header("X-CSRF-Token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
}
