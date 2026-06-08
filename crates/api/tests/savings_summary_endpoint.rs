//! Integration tests for the savings dashboard endpoint (P5-T4, issue #35).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Seeds `savings_events` rows
//! directly (one row per list) with known figures and asserts that
//! `GET /api/savings/summary`:
//!   * groups gross/accepted/net per `"YYYY-MM"` month (NULL net/accepted → 0),
//!     ordered chronologically, with a per-month list count;
//!   * reports `cumulative_net_cents` as the SUM of `net_saving_cents` over the
//!     NON-NULL rows only (the un-reconciled list contributes nothing);
//!   * lists each archived list's outcome newest-first, exposing a null
//!     `chosen_basket_cost_cents` for lists awaiting actual-spend entry (AC3);
//!   * scopes everything to the caller's household (no cross-household leakage);
//!   * requires a session (401 without one).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
const HOUSEHOLD: &str = "11111111-1111-1111-1111-111111111111";

/// An authenticated session. `GET /api/savings/summary` is a read, so no CSRF
/// token is needed here — only the session cookie is carried on requests.
struct Session {
    session: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed a household (id parametrised so we can model a second household), a
/// member user, and log them in.
async fn seed_household_and_user(
    pool: &SqlitePool,
    household_id: &str,
    user_id: &str,
    email: &str,
) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(household_id)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");

    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?1, ?4)",
    )
    .bind(user_id)
    .bind(email)
    .bind(&hash)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'owner', ?3)",
    )
    .bind(household_id)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");

    login(pool, email).await
}

/// Seed an ARCHIVED list owned by `household_id`.
async fn seed_list(pool: &SqlitePool, household_id: &str, list_id: &str, name: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, ?3, 'archived', 0, ?4)",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(name)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Seed one `savings_events` row with explicit figures. `chosen`/`net` are
/// nullable so a test can model a list still awaiting actual-spend entry.
#[allow(clippy::too_many_arguments)]
async fn seed_event(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    created_at: &str,
    gross: i64,
    accepted: Option<i64>,
    chosen: Option<i64>,
    net: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO savings_events \
             (id, list_id, recommendation_run_id, baseline_cost_cents, recommended_cost_cents, \
              theoretical_minimum_cost_cents, chosen_basket_cost_cents, gross_saving_cents, \
              accepted_savings_cents, net_saving_cents, created_at) \
         VALUES (?1, ?2, NULL, 5000, 4200, 3900, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(id)
    .bind(list_id)
    .bind(chosen)
    .bind(gross)
    .bind(accepted)
    .bind(net)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("insert savings event");
}

async fn login(pool: &SqlitePool, email: &str) -> Session {
    let body = serde_json::json!({ "email": email, "password": PASSWORD }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login should succeed");
    Session {
        session: cookie(&resp, "session"),
    }
}

fn cookie(resp: &axum::response::Response, name: &str) -> String {
    for value in resp.headers().get_all(header::SET_COOKIE) {
        let s = value.to_str().unwrap();
        let first = s.split(';').next().unwrap();
        if let Some((k, v)) = first.split_once('=') {
            if k.trim() == name {
                return v.trim().to_string();
            }
        }
    }
    panic!("cookie {name} not set");
}

/// GET /api/savings/summary with an authenticated session.
async fn get_summary(pool: &SqlitePool, session: &Session) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/savings/summary")
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Testing expectation: seed 3 savings events with known values, call the
/// summary endpoint, and assert the monthly aggregates + cumulative figure.
///
/// Fixtures (same household):
///   * list-1, 2026-05: gross 3000, accepted 2800, chosen 4500, net 2500
///   * list-2, 2026-05: gross 1000, accepted  900, chosen 4800, net  800
///   * list-3, 2026-06: gross 2000, accepted NULL, chosen NULL, net NULL (no
///     actual-spend yet)
/// Expected:
///   * monthly[2026-05] = gross 4000, accepted 3700, net 3300, lists 2
///   * monthly[2026-06] = gross 2000, accepted    0, net    0, lists 1
///   * cumulative_net   = 2500 + 800 = 3300 (the NULL-net June row is excluded)
#[tokio::test]
async fn summary_aggregates_three_events() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(&pool, HOUSEHOLD, "list-1", "May A").await;
    seed_list(&pool, HOUSEHOLD, "list-2", "May B").await;
    seed_list(&pool, HOUSEHOLD, "list-3", "June A").await;

    seed_event(
        &pool,
        "ev-1",
        "list-1",
        "2026-05-03T10:00:00Z",
        3000,
        Some(2800),
        Some(4500),
        Some(2500),
    )
    .await;
    seed_event(
        &pool,
        "ev-2",
        "list-2",
        "2026-05-20T10:00:00Z",
        1000,
        Some(900),
        Some(4800),
        Some(800),
    )
    .await;
    seed_event(
        &pool,
        "ev-3",
        "list-3",
        "2026-06-01T10:00:00Z",
        2000,
        None,
        None,
        None,
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    // Cumulative net: only the two non-null rows count (AC2).
    assert_eq!(body["cumulative_net_cents"], 3300);

    // Monthly aggregates, ordered chronologically.
    let monthly = body["monthly"].as_array().expect("monthly array");
    assert_eq!(monthly.len(), 2, "two distinct months");

    assert_eq!(monthly[0]["month"], "2026-05");
    assert_eq!(monthly[0]["gross_cents"], 4000);
    assert_eq!(monthly[0]["accepted_cents"], 3700);
    assert_eq!(monthly[0]["net_cents"], 3300);
    assert_eq!(monthly[0]["lists"], 2);

    assert_eq!(monthly[1]["month"], "2026-06");
    assert_eq!(monthly[1]["gross_cents"], 2000);
    // NULL accepted/net for June coalesce to 0, not skewing the month.
    assert_eq!(monthly[1]["accepted_cents"], 0);
    assert_eq!(monthly[1]["net_cents"], 0);
    assert_eq!(monthly[1]["lists"], 1);
}

/// AC3: a list with `chosen_basket_cost_cents IS NULL` is surfaced in the
/// per-list outcomes with a null chosen cost (the UI's "Enter actual spend"
/// prompt). Lists are ordered newest-first.
#[tokio::test]
async fn summary_lists_outcomes_newest_first_with_null_chosen() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(&pool, HOUSEHOLD, "list-1", "Older").await;
    seed_list(&pool, HOUSEHOLD, "list-2", "Newer").await;

    // Older list: reconciled (chosen + net present).
    seed_event(
        &pool,
        "ev-1",
        "list-1",
        "2026-05-03T10:00:00Z",
        3000,
        Some(2800),
        Some(4500),
        Some(2500),
    )
    .await;
    // Newer list: awaiting actual-spend (chosen + net NULL).
    seed_event(
        &pool,
        "ev-2",
        "list-2",
        "2026-06-01T10:00:00Z",
        2000,
        None,
        None,
        None,
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    let lists = body["lists"].as_array().expect("lists array");
    assert_eq!(lists.len(), 2);

    // Newest first.
    assert_eq!(lists[0]["list_id"], "list-2");
    assert_eq!(lists[0]["list_name"], "Newer");
    assert_eq!(lists[0]["gross_saving_cents"], 2000);
    assert!(
        lists[0]["chosen_basket_cost_cents"].is_null(),
        "unreconciled list keeps a null chosen cost (drives the prompt)"
    );
    assert!(lists[0]["net_saving_cents"].is_null());

    assert_eq!(lists[1]["list_id"], "list-1");
    assert_eq!(lists[1]["chosen_basket_cost_cents"], 4500);
    assert_eq!(lists[1]["net_saving_cents"], 2500);
}

/// Scoping: another household's savings events never appear in the caller's
/// summary (no cross-household leakage).
#[tokio::test]
async fn summary_is_scoped_to_caller_household() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(&pool, HOUSEHOLD, "list-1", "Mine").await;
    seed_event(
        &pool,
        "ev-1",
        "list-1",
        "2026-05-03T10:00:00Z",
        3000,
        Some(2800),
        Some(4500),
        Some(2500),
    )
    .await;

    // A different household with its own (much larger) savings event.
    seed_household_and_user(&pool, "hh-other", "user-b", "b@example.test").await;
    seed_list(&pool, "hh-other", "list-other", "Theirs").await;
    seed_event(
        &pool,
        "ev-other",
        "list-other",
        "2026-05-04T10:00:00Z",
        99999,
        Some(99999),
        Some(1),
        Some(99999),
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    // Only the caller's single event is reflected.
    assert_eq!(body["cumulative_net_cents"], 2500);
    let monthly = body["monthly"].as_array().unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(monthly[0]["gross_cents"], 3000);
    assert_eq!(monthly[0]["lists"], 1);
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["list_id"], "list-1");
}

/// Determinism: a user who belongs to TWO households (each with its own savings
/// events) gets ONLY the figures of the single household resolved by
/// `ORDER BY household_id LIMIT 1` — never a blend across both memberships. This
/// pins the single-household scoping that mirrors `GET /api/settings`.
#[tokio::test]
async fn summary_picks_single_household_for_multi_household_user() {
    let pool = setup_pool().await;
    // Lower-id household is the one `ORDER BY household_id LIMIT 1` selects.
    let lower = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let higher = "ffffffff-ffff-ffff-ffff-ffffffffffff";

    // Seed the user + first membership via the helper (creates the lower
    // household), then add a second membership to the higher household directly.
    let session = seed_household_and_user(&pool, lower, "user-a", "a@example.test").await;
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Other HH', ?2)")
        .bind(higher)
        .bind(db::now_iso())
        .execute(&pool)
        .await
        .expect("insert second household");
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, 'user-a', 'owner', ?2)",
    )
    .bind(higher)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert second membership");

    // A modest saving in the selected (lower-id) household.
    seed_list(&pool, lower, "list-lower", "Selected").await;
    seed_event(
        &pool,
        "ev-lower",
        "list-lower",
        "2026-05-03T10:00:00Z",
        3000,
        Some(2800),
        Some(4500),
        Some(2500),
    )
    .await;
    // A much larger saving in the OTHER (higher-id) household that must be excluded.
    seed_list(&pool, higher, "list-higher", "Excluded").await;
    seed_event(
        &pool,
        "ev-higher",
        "list-higher",
        "2026-05-04T10:00:00Z",
        99999,
        Some(99999),
        Some(1),
        Some(99999),
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    // Only the lower-id household's single event is reflected — not the blend.
    assert_eq!(body["cumulative_net_cents"], 2500);
    let monthly = body["monthly"].as_array().unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(monthly[0]["gross_cents"], 3000);
    assert_eq!(monthly[0]["lists"], 1);
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["list_id"], "list-lower");
}

/// An empty household (no savings events) gets an empty summary, not an error.
#[tokio::test]
async fn summary_empty_household_is_zeroes() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["cumulative_net_cents"], 0);
    assert!(body["monthly"].as_array().unwrap().is_empty());
    assert!(body["lists"].as_array().unwrap().is_empty());
}

/// Auth: a request without a session is a 401 (the extractor).
#[tokio::test]
async fn summary_without_session_is_401() {
    let pool = setup_pool().await;
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/savings/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
