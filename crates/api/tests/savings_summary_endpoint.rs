//! Integration tests for the savings dashboard endpoint (P5-T4, #35; relabelled
//! and made verified-only in #117).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Seeds `savings_events` rows
//! directly (one row per list) with known figures and statuses and asserts that
//! `GET /api/savings/summary`:
//!   * counts ONLY `verified` events in the aggregate it calls saved
//!     (`saved.cumulative_cents` / `saved.monthly`), which today is empty;
//!   * presents `legacy_total_only` (manual whole-basket) events separately
//!     under `legacy`, with their original figures — never as saved money;
//!   * excludes `estimated`, `unreconciled`, and `unchecked` events from both
//!     aggregates (none is verified or a total-only variance);
//!   * groups both aggregates by the list's ARCHIVE date (`shopping_lists.
//!     archived_at`), not the row's creation timestamp;
//!   * lists each event's outcome newest-archive-date-first, carrying the status
//!     and its short baseline/coverage/costs labels, and a null chosen cost for
//!     a list awaiting actual-spend entry;
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

/// Seed an ARCHIVED list owned by `household_id`, with an explicit archive date
/// (the date the summary groups realised figures by).
async fn seed_list(
    pool: &SqlitePool,
    household_id: &str,
    list_id: &str,
    name: &str,
    archived_at: &str,
) {
    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at, archived_at) \
         VALUES (?1, ?2, ?3, 'archived', 0, ?4, ?4)",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(name)
    .bind(archived_at)
    .execute(pool)
    .await
    .expect("insert list");
}

/// Seed one `savings_events` row with explicit figures and status.
///
/// `baseline` is parametrised because `accepted_cents` is derived as
/// `baseline − chosen_basket` over reconciled rows; the unused
/// `accepted_savings_cents` column is deliberately left NULL (no production path
/// writes it this phase).
#[allow(clippy::too_many_arguments)]
async fn seed_event(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    created_at: &str,
    status: &str,
    baseline: i64,
    gross: i64,
    chosen: Option<i64>,
    net: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO savings_events \
             (id, list_id, recommendation_run_id, baseline_cost_cents, recommended_cost_cents, \
              theoretical_minimum_cost_cents, chosen_basket_cost_cents, gross_saving_cents, \
              net_saving_cents, created_at, status) \
         VALUES (?1, ?2, NULL, ?3, 4200, 3900, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(id)
    .bind(list_id)
    .bind(baseline)
    .bind(chosen)
    .bind(gross)
    .bind(net)
    .bind(created_at)
    .bind(status)
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

/// A verified-only aggregate: the saved headline and its monthly buckets count
/// ONLY `verified` rows. An `estimated` opportunity, an `unreconciled` refusal
/// and a `legacy_total_only` total are all excluded.
#[tokio::test]
async fn saved_aggregate_counts_only_verified_rows() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-v",
        "Verified",
        "2026-05-03T10:00:00Z",
    )
    .await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-e",
        "Estimated",
        "2026-05-04T10:00:00Z",
    )
    .await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-u",
        "Unreconciled",
        "2026-05-05T10:00:00Z",
    )
    .await;
    seed_list(&pool, HOUSEHOLD, "list-l", "Legacy", "2026-05-06T10:00:00Z").await;

    seed_event(
        &pool,
        "ev-v",
        "list-v",
        "2026-05-03T10:00:00Z",
        "verified",
        5000,
        3000,
        Some(4500),
        Some(2500),
    )
    .await;
    seed_event(
        &pool,
        "ev-e",
        "list-e",
        "2026-05-04T10:00:00Z",
        "estimated",
        5000,
        3000,
        None,
        None,
    )
    .await;
    seed_event(
        &pool,
        "ev-u",
        "list-u",
        "2026-05-05T10:00:00Z",
        "unreconciled",
        5000,
        4000,
        None,
        None,
    )
    .await;
    seed_event(
        &pool,
        "ev-l",
        "list-l",
        "2026-05-06T10:00:00Z",
        "legacy_total_only",
        5000,
        3000,
        Some(4800),
        Some(700),
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    // Saved: only the verified row, all in one month.
    assert_eq!(body["saved"]["cumulative_cents"], 2500);
    let saved_monthly = body["saved"]["monthly"].as_array().expect("saved monthly");
    assert_eq!(saved_monthly.len(), 1);
    assert_eq!(saved_monthly[0]["month"], "2026-05");
    assert_eq!(saved_monthly[0]["gross_cents"], 3000);
    assert_eq!(saved_monthly[0]["accepted_cents"], 500);
    assert_eq!(saved_monthly[0]["net_cents"], 2500);
    assert_eq!(saved_monthly[0]["lists"], 1, "only the verified row");

    // Legacy: only the total-only row, with baseline, manual costs and variance.
    assert_eq!(body["legacy"]["cumulative_cents"], 700);
    let legacy_monthly = body["legacy"]["monthly"]
        .as_array()
        .expect("legacy monthly");
    assert_eq!(legacy_monthly.len(), 1);
    assert_eq!(legacy_monthly[0]["month"], "2026-05");
    assert_eq!(legacy_monthly[0]["baseline_cents"], 5000);
    assert_eq!(legacy_monthly[0]["manual_total_cents"], 4800);
    assert_eq!(legacy_monthly[0]["travel_cents"], 0);
    assert_eq!(legacy_monthly[0]["variance_cents"], 700);
    assert!(legacy_monthly[0].get("gross_cents").is_none());
    assert_eq!(legacy_monthly[0]["lists"], 1, "only the total-only row");
}

/// A skipped-item manual-total fixture: an event whose whole-basket actual spend
/// makes the stored difference large and positive is NEVER saved money. It shows
/// up only under the legacy label, and the saved aggregate stays empty.
#[tokio::test]
async fn manual_total_never_appears_as_saved_money() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-1",
        "Skipped items",
        "2026-06-01T10:00:00Z",
    )
    .await;
    // Household bought a small basket: €10.00 actual against a €50.00 estimate,
    // so the stored difference is €40.00. It proves nothing about lines skipped.
    seed_event(
        &pool,
        "ev-1",
        "list-1",
        "2026-06-01T11:00:00Z",
        "legacy_total_only",
        5000,
        3000,
        Some(1000),
        Some(1000),
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    assert_eq!(body["saved"]["cumulative_cents"], 0);
    assert!(
        body["saved"]["monthly"].as_array().unwrap().is_empty(),
        "a manual total is never counted as saved money"
    );
    // It is shown, honestly, as a legacy basket variance.
    assert_eq!(body["legacy"]["cumulative_cents"], 1000);
    assert_eq!(body["legacy"]["monthly"][0]["baseline_cents"], 5000);
    assert_eq!(body["legacy"]["monthly"][0]["manual_total_cents"], 1000);
    assert_eq!(body["legacy"]["monthly"][0]["travel_cents"], 0);
    assert_eq!(body["legacy"]["monthly"][0]["variance_cents"], 1000);
    assert!(body["legacy"]["monthly"][0].get("gross_cents").is_none());
}

/// Realised figures group by the list's archive date, not the row's creation
/// timestamp: an event created much later still lands in the shop's month.
#[tokio::test]
async fn realised_figures_group_by_archive_date() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-1",
        "May shop",
        "2026-05-03T10:00:00Z",
    )
    .await;
    seed_event(
        &pool,
        "ev-1",
        "list-1",
        "2026-07-19T10:00:00Z", // created long after the shop
        "legacy_total_only",
        5000,
        3000,
        Some(4500),
        Some(500),
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    let monthly = body["legacy"]["monthly"].as_array().unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(
        monthly[0]["month"], "2026-05",
        "grouped by the archive/shop date, not row creation"
    );
    // The per-event outcome carries the archive date too.
    assert_eq!(body["lists"][0]["archived_at"], "2026-05-03T10:00:00Z");
}

/// Per-event outcomes are newest-archive-date-first, carry the honest status and
/// its short baseline/coverage/costs copy, and expose a null chosen cost for a
/// list awaiting actual-spend.
#[tokio::test]
async fn outcomes_carry_status_labels_and_are_archive_ordered() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-old",
        "Older",
        "2026-05-03T10:00:00Z",
    )
    .await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-new",
        "Newer",
        "2026-06-01T10:00:00Z",
    )
    .await;

    seed_event(
        &pool,
        "ev-old",
        "list-old",
        "2026-05-03T10:00:00Z",
        "legacy_total_only",
        5000,
        3000,
        Some(4500),
        Some(2500),
    )
    .await;
    seed_event(
        &pool,
        "ev-new",
        "list-new",
        "2026-06-01T10:00:00Z",
        "estimated",
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
    // Newest archive date first.
    assert_eq!(lists[0]["list_id"], "list-new");
    assert_eq!(lists[0]["status"], "estimated");
    assert_eq!(
        lists[0]["status_label"],
        "Estimated opportunity (not verified)"
    );
    assert_eq!(
        lists[0]["coverage_label"],
        "Both plans price the same lines; quantities not reconciled"
    );
    assert!(lists[0]["chosen_basket_cost_cents"].is_null());
    assert!(lists[0]["basket_variance_cents"].is_null());

    assert_eq!(lists[1]["list_id"], "list-old");
    assert_eq!(lists[1]["status"], "legacy_total_only");
    assert_eq!(
        lists[1]["status_label"],
        "Legacy total-only basket variance"
    );
    assert_eq!(
        lists[1]["coverage_label"],
        "Whole-basket total only; quantities and purchases unverified"
    );
    assert_eq!(lists[1]["chosen_basket_cost_cents"], 4500);
    assert_eq!(lists[1]["basket_variance_cents"], 2500);

    // Every outcome names its baseline and included costs.
    for outcome in lists {
        assert_eq!(
            outcome["baseline_label"],
            "Baseline: best single store estimate"
        );
        assert_eq!(
            outcome["costs_note"],
            "Travel included where entered; service and data costs excluded."
        );
    }
}

#[tokio::test]
async fn unchecked_rows_are_labelled_and_excluded_from_aggregates() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-unchecked",
        "Unchecked shop",
        "2026-05-03T10:00:00Z",
    )
    .await;
    seed_event(
        &pool,
        "ev-unchecked",
        "list-unchecked",
        "2026-05-03T10:00:00Z",
        "unchecked",
        5000,
        3000,
        None,
        None,
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    assert_eq!(body["saved"]["cumulative_cents"], 0);
    assert!(body["saved"]["monthly"].as_array().unwrap().is_empty());
    assert_eq!(body["legacy"]["cumulative_cents"], 0);
    assert!(body["legacy"]["monthly"].as_array().unwrap().is_empty());
    let outcome = &body["lists"][0];
    assert_eq!(outcome["status"], "unchecked");
    assert_eq!(outcome["status_label"], "Comparability not checked");
    assert_eq!(
        outcome["coverage_label"],
        "No saving claimed: comparability not checked"
    );
}

/// Scoping: another household's savings events never appear in the caller's
/// summary (no cross-household leakage).
#[tokio::test]
async fn summary_is_scoped_to_caller_household() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    seed_list(&pool, HOUSEHOLD, "list-1", "Mine", "2026-05-03T10:00:00Z").await;
    seed_event(
        &pool,
        "ev-1",
        "list-1",
        "2026-05-03T10:00:00Z",
        "legacy_total_only",
        5000,
        3000,
        Some(4500),
        Some(2500),
    )
    .await;

    // A different household with its own (much larger) savings event.
    seed_household_and_user(&pool, "hh-other", "user-b", "b@example.test").await;
    seed_list(
        &pool,
        "hh-other",
        "list-other",
        "Theirs",
        "2026-05-04T10:00:00Z",
    )
    .await;
    seed_event(
        &pool,
        "ev-other",
        "list-other",
        "2026-05-04T10:00:00Z",
        "legacy_total_only",
        100000,
        99999,
        Some(1),
        Some(99999),
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    assert_eq!(body["legacy"]["cumulative_cents"], 2500);
    let monthly = body["legacy"]["monthly"].as_array().unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(monthly[0]["baseline_cents"], 5000);
    assert_eq!(monthly[0]["manual_total_cents"], 4500);
    assert_eq!(monthly[0]["travel_cents"], 0);
    assert_eq!(monthly[0]["variance_cents"], 2500);
    assert!(monthly[0].get("gross_cents").is_none());
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

    // A modest total-only variance in the selected (lower-id) household.
    seed_list(
        &pool,
        lower,
        "list-lower",
        "Selected",
        "2026-05-03T10:00:00Z",
    )
    .await;
    seed_event(
        &pool,
        "ev-lower",
        "list-lower",
        "2026-05-03T10:00:00Z",
        "legacy_total_only",
        5000,
        3000,
        Some(4500),
        Some(2500),
    )
    .await;
    // A much larger one in the OTHER (higher-id) household that must be excluded.
    seed_list(
        &pool,
        higher,
        "list-higher",
        "Excluded",
        "2026-05-04T10:00:00Z",
    )
    .await;
    seed_event(
        &pool,
        "ev-higher",
        "list-higher",
        "2026-05-04T10:00:00Z",
        "legacy_total_only",
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
    assert_eq!(body["legacy"]["cumulative_cents"], 2500);
    let monthly = body["legacy"]["monthly"].as_array().unwrap();
    assert_eq!(monthly.len(), 1);
    assert_eq!(monthly[0]["baseline_cents"], 5000);
    assert_eq!(monthly[0]["manual_total_cents"], 4500);
    assert_eq!(monthly[0]["travel_cents"], 0);
    assert_eq!(monthly[0]["variance_cents"], 2500);
    assert!(monthly[0].get("gross_cents").is_none());
    assert_eq!(monthly[0]["lists"], 1);
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["list_id"], "list-lower");
}

/// An empty household (no savings events) gets an empty, honestly labelled
/// summary, not an error.
#[tokio::test]
async fn summary_empty_household_is_zeroes() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["saved"]["cumulative_cents"], 0);
    assert_eq!(body["saved"]["label"], "Verified savings");
    assert!(body["saved"]["monthly"].as_array().unwrap().is_empty());
    assert_eq!(body["legacy"]["cumulative_cents"], 0);
    assert!(body["legacy"]["monthly"].as_array().unwrap().is_empty());
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
// Real-flow tests: archive → actual-spend → summary
// ---------------------------------------------------------------------------
//
// Integration test files are separate binaries so we can't import the helpers in
// `savings_endpoints.rs`; the small ones we need (seed an ACTIVE list, seed a
// recommendation run, login carrying CSRF, archive via PATCH, POST actual-spend)
// are replicated below.

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

/// Seed one `recommendation_runs` row whose plans_json carries the three plan
/// totals the ledger reads, with the same priced lines on both plans (so the
/// event is `estimated`). Mirrors `savings_endpoints.rs::seed_run`.
async fn seed_recommendation_run(
    pool: &SqlitePool,
    household_id: &str,
    run_id: &str,
    list_id: &str,
    run_at: &str,
) {
    let plan = |total: i64| {
        serde_json::json!({ "stores": [], "lines": [{ "item_index": 0 }], "total_cents": total,
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

/// End-to-end: seed a run, archive the list, POST a whole-basket actual spend,
/// then GET the summary. The event starts as an `estimated` opportunity and, once
/// a total is entered, becomes a `legacy_total_only` basket variance shown under
/// the legacy label — never saved money.
#[tokio::test]
async fn actual_spend_becomes_legacy_variance_through_summary() {
    let pool = setup_pool().await;
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

    let resp = get_summary(
        &pool,
        &Session {
            session: session.clone(),
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    // Nothing is claimed as saved.
    assert_eq!(body["saved"]["cumulative_cents"], 0);
    assert!(body["saved"]["monthly"].as_array().unwrap().is_empty());

    // The total-only variance is shown under the legacy label, grouped by the
    // archive/shop date (set to now during the PATCH).
    let expected_variance = FLOW_BASELINE - chosen - travel; // 200
    assert_eq!(body["legacy"]["cumulative_cents"], expected_variance);
    let monthly = body["legacy"]["monthly"]
        .as_array()
        .expect("legacy monthly");
    assert_eq!(monthly.len(), 1, "one month");
    let expected_month = chrono::Utc::now().format("%Y-%m").to_string();
    assert_eq!(monthly[0]["month"], expected_month);
    assert_eq!(monthly[0]["baseline_cents"], FLOW_BASELINE);
    assert_eq!(monthly[0]["manual_total_cents"], chosen);
    assert_eq!(monthly[0]["travel_cents"], travel);
    assert_eq!(monthly[0]["variance_cents"], expected_variance);
    assert!(monthly[0].get("gross_cents").is_none());

    // The outcome is labelled as a legacy total-only variance.
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["status"], "legacy_total_only");
    assert_eq!(lists[0]["basket_variance_cents"], expected_variance);
}

/// Real-flow coverage guard: a run whose plans price different lines produces an
/// `unreconciled` event through the archive path, so it never enters the saved
/// aggregate even before any actual spend is entered.
#[tokio::test]
async fn mismatched_coverage_archive_stays_out_of_saved() {
    let pool = setup_pool().await;
    seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;
    let (session, csrf) = login_with_csrf(&pool, "a@example.test").await;

    seed_active_list(&pool, HOUSEHOLD, "list-1", "Weekly").await;
    // Best single prices two lines; the recommended plan prices one.
    let plan = |total: i64, lines: Value| {
        serde_json::json!({ "stores": [], "lines": lines, "total_cents": total,
            "missing_count": 0, "dunnes_voucher_offer_text": null })
    };
    let plans_json = serde_json::json!({
        "best_single": plan(FLOW_BASELINE, serde_json::json!([{ "item_index": 0 }, { "item_index": 1 }])),
        "best_two_store": Value::Null,
        "theoretical_minimum": plan(FLOW_THEORETICAL, serde_json::json!([{ "item_index": 0 }, { "item_index": 1 }])),
        "recommended": {
            "kind": "single_store",
            "plan": plan(FLOW_RECOMMENDED, serde_json::json!([{ "item_index": 0 }])),
            "clears_convenience_threshold": false,
            "two_store_saving_cents": 0,
        },
    })
    .to_string();
    sqlx::query(
        "INSERT INTO recommendation_runs \
             (id, list_id, household_id, run_at, offer_snapshot_date, plans_json, \
              recommended_plan, convenience_threshold_2, convenience_threshold_3) \
         VALUES ('run-1', 'list-1', ?1, '2026-06-01T10:00:00Z', NULL, ?2, 'single_store', 1000, 2500)",
    )
    .bind(HOUSEHOLD)
    .bind(&plans_json)
    .execute(&pool)
    .await
    .expect("insert mismatched run");

    let archived = patch_list(
        &pool,
        &session,
        &csrf,
        "list-1",
        serde_json::json!({ "status": "archived" }),
    )
    .await;
    assert_eq!(archived.status(), StatusCode::OK);

    let resp = get_summary(
        &pool,
        &Session {
            session: session.clone(),
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    assert_eq!(body["saved"]["cumulative_cents"], 0);
    assert!(body["saved"]["monthly"].as_array().unwrap().is_empty());
    // Not a total-only variance either — it is an unreconciled refusal.
    assert_eq!(body["legacy"]["cumulative_cents"], 0);
    assert!(body["legacy"]["monthly"].as_array().unwrap().is_empty());
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["status"], "unreconciled");
    assert_eq!(
        lists[0]["status_label"],
        "Not comparable — plans price different lines"
    );
    // The run predates the frozen snapshot, so the outcome has no covered fields
    // and still loads (the full-basket figure is not presented either).
    assert!(lists[0]["covered_subtotal_cents"].is_null());
    assert!(lists[0]["covered_label"].is_null());
    assert!(lists[0]["full_basket_saving_cents"].is_null());
}

/// Issue #117 slice 2: an event's outcome exposes the run's frozen coverage —
/// the bounded covered subtotal, its coverage numerator/denominator and evidence
/// date — while suppressing the full-basket saving when the plans price different
/// lines. A comparable row (or one whose run predates the snapshot) keeps its
/// full-basket figure and carries no covered fields.
#[tokio::test]
async fn outcome_exposes_frozen_covered_subtotal_and_suppresses_full_basket() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, HOUSEHOLD, "user-a", "a@example.test").await;

    // Unreconciled event: a run with a frozen coverage snapshot whose plans price
    // different lines.
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-1",
        "Mismatched",
        "2026-06-10T10:00:00Z",
    )
    .await;
    let plans_json = serde_json::json!({
        "best_single": { "stores": [], "lines": [{ "item_index": 0 }], "total_cents": 5000,
            "missing_count": 0, "dunnes_voucher_offer_text": null },
        "best_two_store": Value::Null,
        "theoretical_minimum": { "stores": [], "lines": [{ "item_index": 0 }], "total_cents": 3900,
            "missing_count": 0, "dunnes_voucher_offer_text": null },
        "recommended": {
            "kind": "two_store",
            "plan": { "stores": [], "lines": [{ "item_index": 0 }, { "item_index": 1 }],
                "total_cents": 4200, "missing_count": 0, "dunnes_voucher_offer_text": null },
            "clears_convenience_threshold": true,
            "two_store_saving_cents": 800
        },
    })
    .to_string();
    let coverage_json = serde_json::json!({
        "version": 1, "basket_lines": 2, "has_best_single": true, "has_recommended": true,
        "recommended_priced": [0, 1], "best_single_priced": [0], "common_scope": [0],
        "best_single_only": [], "recommended_only": [1],
        "missing": [], "unresolved": [], "confirmation_needed": [],
        "covered_subtotal_cents": 3000, "covered_saving_cents": 2000,
        "full_basket_saving_cents": null, "price_date": "2026-06-01",
        "undated_offer_count": 1
    })
    .to_string();
    sqlx::query(
        "INSERT INTO recommendation_runs \
             (id, list_id, household_id, run_at, plans_json, recommended_plan, \
              convenience_threshold_2, convenience_threshold_3, snapshot_version, \
              coverage_json, snapshot_digest) \
         VALUES ('run-1', 'list-1', ?1, '2026-06-01T10:00:00Z', ?2, 'two_store', 1000, 2500, \
                 1, ?3, 'digest')",
    )
    .bind(HOUSEHOLD)
    .bind(&plans_json)
    .bind(&coverage_json)
    .execute(&pool)
    .await
    .expect("insert run");
    sqlx::query(
        "INSERT INTO savings_events \
             (id, list_id, recommendation_run_id, baseline_cost_cents, recommended_cost_cents, \
              theoretical_minimum_cost_cents, gross_saving_cents, created_at, status) \
         VALUES ('ev-1', 'list-1', 'run-1', 5000, 4200, 3900, 800, \
                 '2026-06-10T10:00:00Z', 'unreconciled')",
    )
    .execute(&pool)
    .await
    .expect("insert event");

    // Comparable event whose run predates the snapshot: no covered fields.
    seed_list(
        &pool,
        HOUSEHOLD,
        "list-2",
        "Estimated",
        "2026-06-11T10:00:00Z",
    )
    .await;
    seed_event(
        &pool,
        "ev-2",
        "list-2",
        "2026-06-11T10:00:00Z",
        "estimated",
        5000,
        800,
        None,
        None,
    )
    .await;

    let resp = get_summary(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 2);

    // Newest archive date first: the estimated list-2 leads.
    let estimated = &lists[0];
    assert_eq!(estimated["status"], "estimated");
    assert_eq!(
        estimated["full_basket_saving_cents"], 800,
        "a comparable row keeps its full-basket figure: {estimated:#}"
    );
    assert!(estimated["covered_subtotal_cents"].is_null());
    assert!(estimated["covered_label"].is_null());
    assert!(estimated["undated_offer_count"].is_null());

    let unreconciled = &lists[1];
    assert_eq!(unreconciled["status"], "unreconciled");
    assert!(
        unreconciled["full_basket_saving_cents"].is_null(),
        "the full-basket saving is suppressed on unequal coverage: {unreconciled:#}"
    );
    assert_eq!(unreconciled["covered_subtotal_cents"], 3000);
    assert_eq!(unreconciled["covered_saving_cents"], 2000);
    assert_eq!(unreconciled["covered_lines"], 1);
    assert_eq!(unreconciled["total_lines"], 2);
    assert_eq!(unreconciled["price_date"], "2026-06-01");
    assert_eq!(unreconciled["undated_offer_count"], 1);
    assert_eq!(
        unreconciled["covered_label"],
        "Covered subtotal: 1 of 2 lines, prices as of 2026-06-01, 1 offer undated"
    );
    // The audit gross figure is still stored on the row.
    assert_eq!(unreconciled["gross_saving_cents"], 800);
}
