//! Integration tests for the savings ledger (P5-T3, issue #34).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Covers the issue's
//! acceptance criteria + testing expectations:
//!   * archiving a list WITH a recommendation run creates exactly one
//!     `savings_events` row with the correct baseline/recommended/theoretical/
//!     gross figures;
//!   * archiving a list WITHOUT a recommendation run is a 200 with no row, no
//!     error;
//!   * `POST /api/list/{id}/actual-spend` updates `chosen_basket_cost_cents`,
//!     `travel_cost_cents`, and `net_saving_cents` (= baseline − actual − travel);
//!   * re-archive (archive → reactivate → archive) creates only ONE row;
//!   * actual-spend on a list with no savings_events row is a 404;
//!   * a non-member gets 403 and a CSRF-missing POST is rejected like other
//!     mutations.

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

/// Known plan totals baked into the seeded recommendation run's plans_json.
const BASELINE: i64 = 5000; // best_single.total_cents
const RECOMMENDED: i64 = 4200; // recommended.plan.total_cents
const THEORETICAL: i64 = 3900; // theoretical_minimum.total_cents
const GROSS: i64 = BASELINE - RECOMMENDED;

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed a household, a member user, and log them in.
async fn seed_household_and_user(pool: &SqlitePool, user_id: &str, email: &str) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(HOUSEHOLD)
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
    .bind(HOUSEHOLD)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");

    login(pool, email).await
}

/// Seed an empty ACTIVE list owned by HOUSEHOLD.
async fn seed_list(pool: &SqlitePool, list_id: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'Weekly', 'active', 0, ?3)",
    )
    .bind(list_id)
    .bind(HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Seed one `recommendation_runs` row whose `plans_json` carries the three plan
/// totals the ledger reads. `run_at` is parametrised so tests can order runs.
///
/// The plans_json shape mirrors `optimise::persist_run`: a `best_single` /
/// `theoretical_minimum` carrying `total_cents`, and a `recommended` object
/// nesting `plan.total_cents`. Extra keys the optimiser would emit are omitted —
/// the ledger reader ignores them.
async fn seed_run(
    pool: &SqlitePool,
    run_id: &str,
    list_id: &str,
    run_at: &str,
    best_single: Option<i64>,
    theoretical: Option<i64>,
    recommended: Option<i64>,
) {
    let plan = |total: Option<i64>| match total {
        Some(t) => serde_json::json!({ "stores": [], "lines": [], "total_cents": t,
            "missing_count": 0, "dunnes_voucher_offer_text": null }),
        None => Value::Null,
    };
    let recommended_val = match recommended {
        Some(t) => serde_json::json!({
            "kind": "single_store",
            "plan": plan(Some(t)),
            "clears_convenience_threshold": false,
            "two_store_saving_cents": 0,
        }),
        None => Value::Null,
    };
    let plans_json = serde_json::json!({
        "best_single": plan(best_single),
        "best_two_store": Value::Null,
        "theoretical_minimum": plan(theoretical),
        "recommended": recommended_val,
    })
    .to_string();

    let recommended_plan = if recommended.is_some() {
        "single_store"
    } else {
        "none"
    };

    sqlx::query(
        "INSERT INTO recommendation_runs \
             (id, list_id, household_id, run_at, offer_snapshot_date, plans_json, \
              recommended_plan, convenience_threshold_2, convenience_threshold_3) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, 1000, 2500)",
    )
    .bind(run_id)
    .bind(list_id)
    .bind(HOUSEHOLD)
    .bind(run_at)
    .bind(&plans_json)
    .bind(recommended_plan)
    .execute(pool)
    .await
    .expect("insert recommendation run");
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
        csrf: cookie(&resp, "csrf"),
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

/// PATCH /api/list/{id} with an authenticated session + CSRF header.
async fn patch_list(
    pool: &SqlitePool,
    session: &Session,
    list_id: &str,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/list/{list_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// POST /api/list/{id}/actual-spend with an authenticated session + CSRF header.
async fn post_actual_spend(
    pool: &SqlitePool,
    session: &Session,
    list_id: &str,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/list/{list_id}/actual-spend"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// All `savings_events` rows for a list (id, baseline, recommended, theoretical,
/// gross, chosen, travel, net), for assertions.
type SavingsRow = (
    String,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

async fn savings_rows(pool: &SqlitePool, list_id: &str) -> Vec<SavingsRow> {
    sqlx::query_as::<_, SavingsRow>(
        "SELECT id, baseline_cost_cents, recommended_cost_cents, \
                theoretical_minimum_cost_cents, gross_saving_cents, \
                chosen_basket_cost_cents, travel_cost_cents, net_saving_cents \
         FROM savings_events WHERE list_id = ?1",
    )
    .bind(list_id)
    .fetch_all(pool)
    .await
    .expect("query savings_events")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Acceptance: archiving a list WITH a recommendation run creates exactly one
/// savings_events row with the correct cost figures.
#[tokio::test]
async fn archive_with_run_creates_savings_event() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;

    let resp = patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let rows = savings_rows(&pool, "list-1").await;
    assert_eq!(rows.len(), 1, "exactly one savings_events row");
    let (_, baseline, recommended, theoretical, gross, chosen, travel, net) = &rows[0];
    assert_eq!(*baseline, BASELINE);
    assert_eq!(*recommended, RECOMMENDED);
    assert_eq!(*theoretical, THEORETICAL);
    assert_eq!(*gross, GROSS);
    // chosen/travel/net stay NULL until actual-spend is posted.
    assert!(chosen.is_none(), "chosen_basket NULL before actual-spend");
    assert!(travel.is_none(), "travel NULL before actual-spend");
    assert!(net.is_none(), "net_saving NULL before actual-spend");

    // The row points at the run.
    let run_id: String =
        sqlx::query_scalar("SELECT recommendation_run_id FROM savings_events WHERE list_id = ?1")
            .bind("list-1")
            .fetch_one(&pool)
            .await
            .expect("recommendation_run_id");
    assert_eq!(run_id, "run-1");
}

/// The LATEST run drives the figures when several runs exist for the list.
#[tokio::test]
async fn archive_uses_latest_run() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    // Older run with different totals.
    seed_run(
        &pool,
        "run-old",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(9999),
        Some(8888),
        Some(7777),
    )
    .await;
    // Newer run — this one should win.
    seed_run(
        &pool,
        "run-new",
        "list-1",
        "2026-06-02T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;

    let resp = patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let rows = savings_rows(&pool, "list-1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, BASELINE, "baseline from the latest run");
    let run_id: String =
        sqlx::query_scalar("SELECT recommendation_run_id FROM savings_events WHERE list_id = ?1")
            .bind("list-1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(run_id, "run-new");
}

/// Acceptance: archiving WITHOUT a recommendation run is a 200 with no row, no
/// error.
#[tokio::test]
async fn archive_without_run_creates_no_event() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    let resp = patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(savings_rows(&pool, "list-1").await.is_empty());
}

/// A run with no usable plan (`recommended_plan = 'none'`, null plans) is treated
/// like "no run": archive succeeds, no row inserted.
#[tokio::test]
async fn archive_with_unusable_run_creates_no_event() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-none",
        "list-1",
        "2026-06-01T10:00:00Z",
        None,
        None,
        None,
    )
    .await;

    let resp = patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(savings_rows(&pool, "list-1").await.is_empty());
}

/// Acceptance: actual-spend updates chosen_basket_cost_cents, travel_cost_cents,
/// and net_saving_cents (= baseline − actual − travel).
#[tokio::test]
async fn actual_spend_updates_event() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;
    let archived = patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(archived.status(), StatusCode::OK);

    let actual = 4500;
    let travel = 300;
    let resp = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": actual, "travel_cost_cents": travel }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    let expected_net = BASELINE - actual - travel;
    assert_eq!(body["chosen_basket_cost_cents"], actual);
    assert_eq!(body["travel_cost_cents"], travel);
    assert_eq!(body["net_saving_cents"], expected_net);
    assert_eq!(body["baseline_cost_cents"], BASELINE);

    // Persisted values match.
    let rows = savings_rows(&pool, "list-1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].5, Some(actual));
    assert_eq!(rows[0].6, Some(travel));
    assert_eq!(rows[0].7, Some(expected_net));
}

/// travel_cost_cents defaults to 0 when omitted.
#[tokio::test]
async fn actual_spend_defaults_travel_to_zero() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;
    patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;

    let resp = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": 4800 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["travel_cost_cents"], 0);
    assert_eq!(body["net_saving_cents"], BASELINE - 4800);
}

/// Overspend: when actual + travel exceeds the baseline, net_saving_cents is
/// allowed to go negative and is stored as a negative figure (not clamped).
#[tokio::test]
async fn actual_spend_overspend_stores_negative_net() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;
    patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;

    // Spend more than the baseline so the household overspent.
    let actual = BASELINE + 700;
    let travel = 200;
    let resp = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": actual, "travel_cost_cents": travel }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let expected_net = BASELINE - actual - travel;
    assert!(expected_net < 0, "this fixture must model an overspend");
    let body = json_body(resp).await;
    assert_eq!(body["net_saving_cents"], expected_net);

    // The negative net is persisted, not clamped.
    let rows = savings_rows(&pool, "list-1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].7, Some(expected_net));
}

/// Double-post: a second actual-spend POST overwrites the same row in place
/// (no duplicate, figures reflect the latest submission).
#[tokio::test]
async fn actual_spend_double_post_updates_same_row() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;
    patch_list(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;

    // First submission.
    let first = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": 4500, "travel_cost_cents": 300 }),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_id = json_body(first).await["id"].as_str().unwrap().to_string();

    // Correction: re-post with different figures.
    let second_actual = 4100;
    let second_travel = 150;
    let second = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({
            "actual_spend_cents": second_actual,
            "travel_cost_cents": second_travel,
        }),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = json_body(second).await;
    // Same row (same id), figures now reflect the second submission.
    assert_eq!(second_body["id"].as_str().unwrap(), first_id);
    let expected_net = BASELINE - second_actual - second_travel;
    assert_eq!(second_body["chosen_basket_cost_cents"], second_actual);
    assert_eq!(second_body["travel_cost_cents"], second_travel);
    assert_eq!(second_body["net_saving_cents"], expected_net);

    // Still exactly one row, carrying the latest figures.
    let rows = savings_rows(&pool, "list-1").await;
    assert_eq!(rows.len(), 1, "double-post must not duplicate the row");
    assert_eq!(rows[0].5, Some(second_actual));
    assert_eq!(rows[0].6, Some(second_travel));
    assert_eq!(rows[0].7, Some(expected_net));
}

/// Edge: re-archive (archive → reactivate → archive) creates only ONE row.
#[tokio::test]
async fn re_archive_is_idempotent() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;

    assert_eq!(
        patch_list(
            &pool,
            &session,
            "list-1",
            serde_json::json!({ "status": "archived" })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        patch_list(
            &pool,
            &session,
            "list-1",
            serde_json::json!({ "status": "active" })
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        patch_list(
            &pool,
            &session,
            "list-1",
            serde_json::json!({ "status": "archived" })
        )
        .await
        .status(),
        StatusCode::OK
    );

    assert_eq!(
        savings_rows(&pool, "list-1").await.len(),
        1,
        "re-archive must not duplicate the savings_events row"
    );
}

/// Edge: actual-spend on a list with no savings_events row is a 404.
#[tokio::test]
async fn actual_spend_without_event_is_404() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    // List exists, but was never archived / has no run => no savings_events row.

    let resp = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": 4500 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// actual-spend on a missing list is a 404 (before any savings lookup).
#[tokio::test]
async fn actual_spend_missing_list_is_404() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = post_actual_spend(
        &pool,
        &session,
        "nope",
        serde_json::json!({ "actual_spend_cents": 4500 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A negative spend is a 422 (validated at the boundary).
#[tokio::test]
async fn actual_spend_negative_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    let resp = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": -1 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Auth: a non-member of the list's household gets 403.
#[tokio::test]
async fn actual_spend_non_member_is_forbidden() {
    let pool = setup_pool().await;
    let _owner = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;
    seed_run(
        &pool,
        "run-1",
        "list-1",
        "2026-06-01T10:00:00Z",
        Some(BASELINE),
        Some(THEORETICAL),
        Some(RECOMMENDED),
    )
    .await;

    // A second user in a DIFFERENT household.
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES ('hh-other', 'Other', ?1)")
        .bind(db::now_iso())
        .execute(&pool)
        .await
        .expect("insert other household");
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES ('user-b', 'b@example.test', ?1, 'user-b', ?2)",
    )
    .bind(&hash)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert user-b");
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES ('hh-other', 'user-b', 'owner', ?1)",
    )
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert membership-b");

    let session = login(&pool, "b@example.test").await;
    let resp = post_actual_spend(
        &pool,
        &session,
        "list-1",
        serde_json::json!({ "actual_spend_cents": 4500 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// CSRF: a POST without the CSRF token header is rejected like other mutations.
#[tokio::test]
async fn actual_spend_missing_csrf_is_rejected() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    // Authenticated session cookie, but NO X-CSRF-Token header.
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/list/list-1/actual-spend")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::from(
                    serde_json::json!({ "actual_spend_cents": 4500 }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
