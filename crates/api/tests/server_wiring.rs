//! Integration tests for the P1-T6 server wiring (issue #17).
//!
//! Binds the full application on an OS-assigned port (`127.0.0.1:0`) over a real
//! TCP listener, then smoke-tests every route for the correct HTTP status code
//! via `reqwest`. Most tests bind [`buywise_api::app`] (production frontend dir);
//! only the static-file tests bind [`buywise_api::app_with_frontend`] to point
//! the fallback at a fixture directory:
//!
//!   * the auth routes (`/api/auth/{login,logout,me}`) authenticated and not;
//!   * the list routes (`/api/lists`, `/api/list/:id`, `/api/items`);
//!   * the mutation endpoint (`/api/mutations`);
//!   * the SSE stream (`/api/sse/:list_id`) — checked for a `200 text/event-stream`
//!     handshake only (the body never completes, so the request is dropped after
//!     the headers arrive);
//!   * the optimise route is mounted behind auth (`/api/optimise` → `401` when
//!     unauthenticated; full behaviour in `tests/optimise_endpoint.rs`);
//!   * the static SPA fallback: `GET /` 404s with no `frontend/build`, and serves
//!     the SPA shell (`200`) when a build directory exists.
//!
//! These exercise the acceptance criteria: the server starts against an existing
//! SQLite database, every API route returns a sensible status, and `GET /` serves
//! the app shell once the build directory is present (404 until then).

use std::net::SocketAddr;
use std::time::Duration;

use buywise_api::middleware::{CSRF_COOKIE, CSRF_HEADER};
use buywise_api::{app, app_with_frontend, AppState};
use buywise_core::db;
use sqlx::SqlitePool;

const HOUSEHOLD_ID: &str = "household-1";
const USER_ID: &str = "user-a";
const USER_EMAIL: &str = "alice@example.test";
const PASSWORD: &str = "correct horse battery staple";
const LIST_ID: &str = "list-1";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A shared-cache in-memory pool so the spawned server and the test share rows.
async fn setup_pool() -> SqlitePool {
    let name = format!("buywise_wiring_{}", uuid::Uuid::new_v4().simple());
    let url = format!("file:{name}?mode=memory&cache=shared");
    let pool = db::connect(&url).await.expect("connect shared");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn seed_household_user_and_list(pool: &SqlitePool) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(HOUSEHOLD_ID)
        .bind("Home")
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");

    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(USER_ID)
    .bind(USER_EMAIL)
    .bind(&hash)
    .bind("Alice")
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'owner', ?3)",
    )
    .bind(HOUSEHOLD_ID)
    .bind(USER_ID)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");

    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'Weekly shop', 'active', 0, ?3)",
    )
    .bind(LIST_ID)
    .bind(HOUSEHOLD_ID)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Panic-safe cleanup for a throwaway directory: removes it on drop, so a failed
/// assertion mid-test does not leak the fixture under `std::env::temp_dir()`.
/// Avoids pulling in the `tempfile` dev-dependency for one test.
struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Bind the full app on `127.0.0.1:0`, spawn it, and return the bound address.
async fn spawn_app(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

/// A reqwest client with a cookie jar so the session/CSRF cookies persist across
/// requests, plus a short timeout so a hung request fails the test.
///
/// The API sets the session/CSRF cookies with the `Secure` attribute, which a
/// browser would withhold over plain `http://`. reqwest's cookie jar does **not**
/// enforce `Secure` (it has no notion of a "secure transport"), so the cookies
/// flow back over the test's `http://127.0.0.1` loopback and the authenticated
/// happy path exercises the real routes. This is a deliberate test convenience,
/// not a relaxation of production behaviour — the `Secure` flag is still set on
/// the wire (and must stay set); production serves over HTTPS.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build client")
}

/// Log in and return the CSRF token from the `csrf` Set-Cookie header. The
/// session + csrf cookies are retained in the client's jar for later requests.
async fn login(client: &reqwest::Client, base: &str) -> String {
    let resp = client
        .post(format!("{base}/api/auth/login"))
        .json(&serde_json::json!({ "email": USER_EMAIL, "password": PASSWORD }))
        .send()
        .await
        .expect("login request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "login should succeed"
    );

    let mut csrf = None;
    for value in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        let s = value.to_str().unwrap_or("");
        let first = s.split(';').next().unwrap_or("");
        if let Some((k, v)) = first.split_once('=') {
            if k.trim() == CSRF_COOKIE {
                csrf = Some(v.trim().to_string());
            }
        }
    }
    csrf.expect("login response sets a csrf cookie")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Unauthenticated requests to the protected API surface return 401, and login
/// succeeds — the basic "server is wired and responding" smoke test.
#[tokio::test]
async fn unauthenticated_protected_routes_are_rejected() {
    let pool = setup_pool().await;
    seed_household_user_and_list(&pool).await;
    let addr = spawn_app(app(AppState::new(pool))).await;
    let base = format!("http://{addr}");
    let client = client();

    for path in ["/api/auth/me", "/api/lists", "/api/items"] {
        let resp = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "GET {path} without a session should be 401"
        );
    }

    let resp = client
        .get(format!("{base}/api/list/{LIST_ID}"))
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "GET /api/list/:id without a session should be 401"
    );
}

/// Drive each API route with a real session and assert the expected status.
#[tokio::test]
async fn authenticated_routes_respond_with_expected_status() {
    let pool = setup_pool().await;
    seed_household_user_and_list(&pool).await;
    let addr = spawn_app(app(AppState::new(pool))).await;
    let base = format!("http://{addr}");
    let client = client();

    let csrf = login(&client, &base).await;

    // GET /api/auth/me
    let resp = client
        .get(format!("{base}/api/auth/me"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "GET /api/auth/me");

    // GET /api/lists
    let resp = client
        .get(format!("{base}/api/lists"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "GET /api/lists");

    // GET /api/list/:id (seeded list in the user's household)
    let resp = client
        .get(format!("{base}/api/list/{LIST_ID}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "GET /api/list/:id");

    // GET /api/items
    let resp = client
        .get(format!("{base}/api/items"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "GET /api/items");

    // POST /api/mutations — exercise the route reaches the handler (CSRF + auth
    // pass); a malformed/empty body is rejected by the handler, not the router.
    // We assert it is NOT an auth/CSRF/404 failure: the request was routed and
    // authorised, so the status reflects request validation, not wiring.
    let resp = client
        .post(format!("{base}/api/mutations"))
        .header(CSRF_HEADER, &csrf)
        .json(&serde_json::json!([]))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() != reqwest::StatusCode::NOT_FOUND
            && resp.status() != reqwest::StatusCode::UNAUTHORIZED
            && resp.status() != reqwest::StatusCode::FORBIDDEN,
        "POST /api/mutations routed + authorised (got {})",
        resp.status()
    );

    // GET /api/sse/:list_id — the body is an endless event-stream, so only the
    // handshake is checked. `reqwest` returns once headers arrive; dropping the
    // response closes the connection.
    let resp = client
        .get(format!("{base}/api/sse/{LIST_ID}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "GET /api/sse/:list_id handshake"
    );
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.starts_with("text/event-stream")),
        Some(true),
        "SSE content-type"
    );
    drop(resp);

    // POST /api/logout — CSRF-protected, behind auth. 204 No Content on success.
    let resp = client
        .post(format!("{base}/api/auth/logout"))
        .header(CSRF_HEADER, &csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NO_CONTENT,
        "POST /api/auth/logout"
    );
}

/// The optimise route (P4-T1) is wired behind the `AuthenticatedUser` extractor,
/// so an unauthenticated POST is rejected with `401 Unauthorized` (it no longer
/// returns the Phase-1 `501` stub). The full behaviour is covered by
/// `tests/optimise_endpoint.rs`; this only asserts the route is mounted and
/// guarded, not bypassable as an anonymous endpoint.
#[tokio::test]
async fn optimise_route_requires_authentication() {
    let pool = setup_pool().await;
    let addr = spawn_app(app(AppState::new(pool))).await;
    let base = format!("http://{addr}");
    let client = client();

    let resp = client
        .post(format!("{base}/api/optimise"))
        .json(&serde_json::json!({ "list_id": "whatever" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "POST /api/optimise without a session"
    );
}

/// A known API path hit with the wrong method returns `405 Method Not Allowed`
/// from axum's method router — it must NOT fall through to the static SPA
/// fallback (which would mask the API with a `200`/HTML shell). This is the key
/// "does the catch-all fallback shadow the API?" assertion: `/api/mutations` is
/// POST-only, so `GET` exercises the precedence of the API routes over the
/// `fallback_service`.
#[tokio::test]
async fn wrong_method_on_api_route_is_405_not_spa_fallback() {
    let pool = setup_pool().await;
    let addr = spawn_app(app(AppState::new(pool))).await;
    let base = format!("http://{addr}");
    let client = client();

    let resp = client
        .get(format!("{base}/api/mutations"))
        .send()
        .await
        .expect("request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED,
        "GET on POST-only /api/mutations must be 405, not the SPA fallback"
    );

    // Defensively confirm we did not receive the SPA shell HTML.
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.text().await.unwrap_or_default();
    assert!(
        !body.contains("<!doctype html"),
        "405 response must not be the SPA shell (content-type {content_type:?})"
    );
}

/// With no `frontend/build` directory, the SPA fallback 404s (Phase 2 ships the
/// build). The API routes still take precedence and respond normally.
#[tokio::test]
async fn static_fallback_404s_without_a_build() {
    let pool = setup_pool().await;
    // Point at a directory guaranteed not to exist.
    let missing = std::env::temp_dir().join(format!("buywise-no-build-{}", uuid::Uuid::new_v4()));
    let addr = spawn_app(app_with_frontend(AppState::new(pool), &missing)).await;
    let base = format!("http://{addr}");
    let client = client();

    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "GET / 404s when frontend/build is absent"
    );
}

/// When a build directory exists, `GET /` serves the SPA shell and an unknown
/// client-side route falls back to the same shell (SPA mode).
#[tokio::test]
async fn static_fallback_serves_spa_shell_when_built() {
    let pool = setup_pool().await;

    // Create a throwaway build dir with an index + SPA fallback shell. The guard
    // removes it on drop, even if an assertion below panics mid-test.
    let build_dir =
        std::env::temp_dir().join(format!("buywise-build-{}", uuid::Uuid::new_v4().simple()));
    let _build_guard = TempDirGuard(build_dir.clone());
    std::fs::create_dir_all(&build_dir).expect("mkdir build");
    std::fs::write(
        build_dir.join("index.html"),
        "<!doctype html><title>BuyWise</title>",
    )
    .expect("write index");
    std::fs::write(
        build_dir.join("200.html"),
        "<!doctype html><title>BuyWise SPA</title>",
    )
    .expect("write spa shell");

    let addr = spawn_app(app_with_frontend(AppState::new(pool), &build_dir)).await;
    let base = format!("http://{addr}");
    let client = client();

    // GET / serves index.html.
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "GET / served");
    let body = resp.text().await.unwrap();
    assert!(body.contains("BuyWise"), "index.html body served");

    // An unknown deep link falls back to the SPA shell (200.html), not 404.
    let resp = client
        .get(format!("{base}/lists/abc/deep/link"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "deep link falls back to SPA shell"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("SPA"), "200.html shell served on fallback");

    // An UNKNOWN /api/... path must still 404 even with a build present — it must
    // NOT fall through to the SPA shell. This is the companion to the wrong-method
    // test: that covers known paths; this covers unrouted API paths, which would
    // otherwise be swallowed by the `fallback_service` once `200.html` exists.
    let resp = client
        .get(format!("{base}/api/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "unknown /api/... path must 404, not serve the SPA shell"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        !body.contains("SPA") && !body.contains("<!doctype html"),
        "unknown /api/... 404 must not be the SPA shell"
    );

    // The bare `/api` path (no trailing segment, so not covered by the
    // `/api/{*rest}` catch-all) must also 404 rather than serve the SPA shell.
    let resp = client.get(format!("{base}/api")).send().await.unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NOT_FOUND,
        "bare /api path must 404, not serve the SPA shell"
    );
    let body = resp.text().await.unwrap_or_default();
    assert!(
        !body.contains("SPA") && !body.contains("<!doctype html"),
        "bare /api 404 must not be the SPA shell"
    );

    // `_build_guard` removes the fixture directory on drop.
}
