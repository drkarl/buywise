//! Integration tests for the savings dashboard endpoint (P5-T4, issue #35).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Seeds `savings_events` rows
//! directly (one row per list) with known figures and asserts that
//! `GET /api/savings/summary`:
//!   * groups gross/accepted/net per `"YYYY-MM"` month, ordered chronologically,
//!     with a per-month list count. `accepted_cents` is DERIVED as
//!     `baseline − chosen_basket` over reconciled rows (un-reconciled → 0), so
//!     these tests seed `baseline`/`chosen` rather than the unused
//!     `accepted_savings_cents` column; `net_saving_cents` NULL → 0;
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
///
/// `baseline` is parametrised because the summary derives `accepted_cents` as
/// `baseline − chosen_basket` over reconciled rows; the unused
/// `accepted_savings_cents` column is deliberately left NULL (no production path
/// writes it this phase).
#[allow(clippy::too_many_arguments)]
async fn seed_event(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    created_at: &str,
    baseline: i64,
    gross: i64,
    chosen: Option<i64>,
    net: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO savings_events \
             (id, list_id, recommendation_run_id, baseline_cost_cents, recommended_cost_cents, \
              theoretical_minimum_cost_cents, chosen_basket_cost_cents, gross_saving_cents, \
              net_saving_cents, created_at) \
         VALUES (?1, ?2, NULL, ?3, 4200, 3900, ?4, ?5, ?6, ?7)",
    )
    .bind(id)
    .bind(list_id)
    .bind(baseline)
    .bind(chosen)
    .bind(gross)
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
/// Fixtures (same household, all baseline 5000):
///   * list-1, 2026-05: gross 3000, chosen 4500, net 2500 → accepted 5000−4500=500
///   * list-2, 2026-05: gross 1000, chosen 4800, net  800 → accepted 5000−4800=200
///   * list-3, 2026-06: gross 2000, chosen NULL, net NULL (no actual-spend yet) →
///     accepted 0
/// Expected:
///   * monthly[2026-05] = gross 4000, accepted 700, net 3300, lists 2
///   * monthly[2026-06] = gross 2000, accepted   0, net    0, lists 1
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
        5000,
        3000,
        Some(4500),
        Some(2500),
    )
    .await;
    seed_event(
        &pool,
        "ev-2",
        "list-2",
        "2026-05-20T10:00:00Z",
        5000,
        1000,
        Some(4800),
        Some(800),
    )
    .await;
    seed_event(
        &pool,
        "ev-3",
        "list-3",
        "2026-06-01T10:00:00Z",
        5000,
        2000,
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
    // Derived accepted = (5000−4500) + (5000−4800) = 500 + 200 = 700.
    assert_eq!(monthly[0]["accepted_cents"], 700);
    assert_eq!(monthly[0]["net_cents"], 3300);
    assert_eq!(monthly[0]["lists"], 2);

    assert_eq!(monthly[1]["month"], "2026-06");
    assert_eq!(monthly[1]["gross_cents"], 2000);
    // The un-reconciled June row (chosen NULL) contributes 0 to accepted and net.
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
        5000,
        3000,
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
        5000,
        2000,
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
        5000,
        3000,
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
        100000,
        99999,
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
        5000,
        3000,
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
        100000,
        99999,
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

// ---------------------------------------------------------------------------
// Real-flow test: archive → actual-spend → summary
// ---------------------------------------------------------------------------
//
// The reviewer (P5-T4) asked for a test that exercises the real production path
// rather than seeding figures directly, to prove `accepted_cents` is non-zero
// through that path (it would always read 0 if it summed the never-written
// `accepted_savings_cents` column). Integration test files are separate binaries
// so we can't import the helpers in `savings_endpoints.rs`; the small ones we
// need (seed an ACTIVE list, seed a recommendation run, login carrying CSRF,
// archive via PATCH, POST actual-spend) are replicated below, mirroring the seed
// shapes in that file.

/// Known plan totals baked into the seeded run's plans_json (mirror
/// `savings_endpoints.rs`).
const FLOW_BASELINE: i64 = 5000; // best_single.total_cents
const FLOW_RECOMMENDED: i64 = 4200; // recommended.plan.total_cents
const FLOW_THEORETICAL: i64 = 3900; // theoretical_minimum.total_cents

/// Log in and capture BOTH the session and CSRF cookies (the archive PATCH and
/// actual-spend POST are mutations requiring CSRF).
async fn login_with_csrf(pool: &SqlitePool, email: &str) -> (String, String) {
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
    (cookie(&resp, "session"), cookie(&resp, "csrf"))
}

/// Seed an empty ACTIVE list (archive will turn it into a savings event).
async fn seed_active_list(pool: &SqlitePool, household_id: &str, list_id: &str, name: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, ?3, 'active', 0, ?4)",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(name)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert active list");
}

/// Seed one `recommendation_runs` row whose `plans_json` carries the three plan
/// totals the ledger reads (mirrors `savings_endpoints.rs::seed_run`).
async fn seed_recommendation_run(
    pool: &SqlitePool,
    household_id: &str,
    run_id: &str,
    list_id: &str,
    run_at: &str,
) {
    let plan = |total: i64| {
        serde_json::json!({ "stores": [], "lines": [], "total_cents": total,
            "missing_count": 0, "dunnes_voucher_offer_text": null })
    };
    let plans_json = serde_json::json!({
        "best_single": plan(FLOW_BASELINE),
        "best_two_store": Value::Null,
        "theoretical_minimum": plan(FLOW_THEORETICAL),
        "recommended": {
            "kind": "single_store",
            "plan": plan(FLOW_RECOMMENDED),
            "clears_convenience_threshold": false,
            "two_store_saving_cents": 0,
        },
    })
    .to_string();

    sqlx::query(
        "INSERT INTO recommendation_runs \
             (id, list_id, household_id, run_at, offer_snapshot_date, plans_json, \
              recommended_plan, convenience_threshold_2, convenience_threshold_3) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, 'single_store', 1000, 2500)",
    )
    .bind(run_id)
    .bind(list_id)
    .bind(household_id)
    .bind(run_at)
    .bind(&plans_json)
    .execute(pool)
    .await
    .expect("insert recommendation run");
}

/// PATCH /api/list/{id} with session + CSRF (used to archive).
async fn patch_list(
    pool: &SqlitePool,
    session: &str,
    csrf: &str,
    list_id: &str,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/list/{list_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={session}"))
                .header("X-CSRF-Token", csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// POST /api/list/{id}/actual-spend with session + CSRF.
async fn post_actual_spend(
    pool: &SqlitePool,
    session: &str,
    csrf: &str,
    list_id: &str,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/list/{list_id}/actual-spend"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={session}"))
                .header("X-CSRF-Token", csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// End-to-end: seed a run, archive the list (creating the savings_events row),
/// POST actual-spend, then GET the summary — and assert `accepted_cents` and
/// `net_cents` are BOTH non-zero and equal the derived `baseline − chosen` and
/// `baseline − chosen − travel`. This proves accepted no longer reads as 0
/// through the real path (it would, if the query summed the never-written
/// `accepted_savings_cents` column).
#[tokio::test]
async fn summary_accepted_is_nonzero_through_archive_then_actual_spend() {
    let pool = setup_pool().await;
    // Reuse the read-only helper to seed the household, member, and a session;
    // log in again to also capture the CSRF cookie the mutations need.
    seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    let (session, csrf) = login_with_csrf(&pool, "a@example.test").await;

    seed_active_list(&pool, HOUSEHOLD, "list-1", "Weekly").await;
    seed_recommendation_run(&pool, HOUSEHOLD, "run-1", "list-1", "2026-06-01T10:00:00Z").await;

    // Archive → creates the savings_events row (chosen/net still NULL).
    let archived = patch_list(
        &pool,
        &session,
        &csrf,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(archived.status(), StatusCode::OK);

    // Post the household's actual spend + travel.
    let chosen = 4500;
    let travel = 300;
    let spend = post_actual_spend(
        &pool,
        &session,
        &csrf,
        "list-1",
        serde_json::json!({ "actual_spend_cents": chosen, "travel_cost_cents": travel }),
    )
    .await;
    assert_eq!(spend.status(), StatusCode::OK);

    // GET the summary and assert the DERIVED accepted + stored net are both > 0.
    let resp = get_summary(
        &pool,
        &Session {
            session: session.clone(),
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    let expected_accepted = FLOW_BASELINE - chosen; // 500
    let expected_net = FLOW_BASELINE - chosen - travel; // 200
    assert!(
        expected_accepted > 0 && expected_net > 0,
        "fixture must be positive"
    );

    // The savings_events row this flow creates is stamped with the server's
    // `now()` at archive time, so the aggregate month is the CURRENT UTC month —
    // never a fixed literal (this assertion hardcoded "2026-06" and rotted once
    // the wall clock moved past it).
    let expected_month = chrono::Utc::now().format("%Y-%m").to_string();
    assert_eq!(monthly[0]["month"], expected_month);
    // The regression this guards: accepted is non-zero (would be 0 if the query
    // summed the never-written accepted_savings_cents column).
    assert_eq!(monthly[0]["accepted_cents"], expected_accepted);
    assert_eq!(monthly[0]["net_cents"], expected_net);
    assert_eq!(body["cumulative_net_cents"], expected_net);
}
