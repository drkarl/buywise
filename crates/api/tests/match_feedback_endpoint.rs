//! Integration tests for `POST /api/match-feedback` (P5-T1, issue #32).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Each test seeds its own
//! household, a member user, a canonical item, and a retailer product, logs in,
//! and exercises the match-feedback write path.
//!
//! Coverage (the issue's acceptance criteria + testing expectations):
//!   * accept → `product_matches.status = 'accepted'`;
//!   * reject → `product_matches.status = 'rejected'`;
//!   * accept-then-reject the SAME match leaves exactly ONE `product_matches` row
//!     (status `rejected`) and TWO `match_feedback` rows (history preserved);
//!   * a `match_feedback` row is always inserted;
//!   * `prefer` maps the status to `accepted` while recording the verbatim action;
//!   * an unknown `action` is 422;
//!   * a non-member caller is 404 (never leaking the other household's catalogue);
//!   * a missing CSRF header is 403, no session is 401;
//!   * an unknown canonical item / retailer product is 404.

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
const OTHER_HOUSEHOLD: &str = "22222222-2222-2222-2222-222222222222";
const CATEGORY: &str = "cat-dairy";
const MILK_ID: &str = "ci-milk";
const PRODUCT_ID: &str = "rp-milk-tesco";

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed a household with the given id.
async fn seed_household(pool: &SqlitePool, household_id: &str) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(household_id)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");
}

/// Seed a user, make them a member of `household_id`, and log them in.
async fn seed_member_and_login(
    pool: &SqlitePool,
    user_id: &str,
    email: &str,
    household_id: &str,
) -> Session {
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

/// Seed one category + one canonical item (Milk) in `household_id`.
async fn seed_canonical_item(pool: &SqlitePool, item_id: &str, household_id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES (?1, 'Dairy', 'Dairy', 'icon', 1)",
    )
    .bind(CATEGORY)
    .execute(pool)
    .await
    .expect("insert category");

    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES (?1, ?2, 'Milk', 'Leche', ?3, 'any', ?4)",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(CATEGORY)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");
}

/// Seed one (global) retailer product.
async fn seed_retailer_product(pool: &SqlitePool, product_id: &str) {
    sqlx::query(
        "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, pack_count, first_seen_at, last_seen_at) \
         VALUES (?1, 'basketwatch', 'tesco', ?1, ?1, 1, ?2, ?2)",
    )
    .bind(product_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert retailer product");
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

/// POST /api/match-feedback with a session + CSRF header.
async fn post_feedback(
    pool: &SqlitePool,
    session: &Session,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/match-feedback")
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

/// Standard happy-path fixture: household, member user (logged in), one canonical
/// item, one retailer product.
async fn seed_happy_path(pool: &SqlitePool) -> Session {
    seed_household(pool, HOUSEHOLD).await;
    let session = seed_member_and_login(pool, "user-a", "a@example.test", HOUSEHOLD).await;
    seed_canonical_item(pool, MILK_ID, HOUSEHOLD).await;
    seed_retailer_product(pool, PRODUCT_ID).await;
    session
}

fn accept_body() -> Value {
    serde_json::json!({
        "canonical_item_id": MILK_ID,
        "retailer_product_id": PRODUCT_ID,
        "action": "accept"
    })
}

async fn match_status(pool: &SqlitePool) -> Option<String> {
    sqlx::query_scalar(
        "SELECT status FROM product_matches \
         WHERE household_id = ?1 AND canonical_item_id = ?2 AND retailer_product_id = ?3",
    )
    .bind(HOUSEHOLD)
    .bind(MILK_ID)
    .bind(PRODUCT_ID)
    .fetch_optional(pool)
    .await
    .expect("query status")
}

async fn match_row_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM product_matches \
         WHERE household_id = ?1 AND canonical_item_id = ?2 AND retailer_product_id = ?3",
    )
    .bind(HOUSEHOLD)
    .bind(MILK_ID)
    .bind(PRODUCT_ID)
    .fetch_one(pool)
    .await
    .expect("count matches")
}

async fn feedback_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM match_feedback")
        .fetch_one(pool)
        .await
        .expect("count feedback")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Accepting a match upserts `product_matches.status = 'accepted'` and inserts one
/// `match_feedback` row (AC #1, AC #3).
#[tokio::test]
async fn accept_sets_status_accepted_and_records_feedback() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let resp = post_feedback(&pool, &session, accept_body()).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json_body(resp).await;
    assert_eq!(body["status"], "accepted");
    assert_eq!(body["action"], "accept");

    assert_eq!(match_status(&pool).await.as_deref(), Some("accepted"));
    assert_eq!(match_row_count(&pool).await, 1);
    assert_eq!(feedback_count(&pool).await, 1);
}

/// Rejecting a match sets `product_matches.status = 'rejected'` (AC #2).
#[tokio::test]
async fn reject_sets_status_rejected() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let body = serde_json::json!({
        "canonical_item_id": MILK_ID,
        "retailer_product_id": PRODUCT_ID,
        "action": "reject"
    });
    let resp = post_feedback(&pool, &session, body).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(json_body(resp).await["status"], "rejected");

    assert_eq!(match_status(&pool).await.as_deref(), Some("rejected"));
    assert_eq!(feedback_count(&pool).await, 1);
}

/// The acceptance test: accept then reject the SAME match → exactly ONE
/// `product_matches` row with status `rejected`, and TWO `match_feedback` rows
/// (history preserved). Also asserts the upsert kept the SAME `product_matches.id`
/// across the update so the `match_feedback` FKs stay valid (AC #4, the `INSERT OR
/// REPLACE` pitfall the design doc warns against).
#[tokio::test]
async fn accept_then_reject_same_match_keeps_one_row_two_feedback() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let accept = post_feedback(&pool, &session, accept_body()).await;
    assert_eq!(accept.status(), StatusCode::CREATED);
    let accept_id = json_body(accept).await["product_match_id"]
        .as_str()
        .expect("product_match_id")
        .to_string();

    let reject_body = serde_json::json!({
        "canonical_item_id": MILK_ID,
        "retailer_product_id": PRODUCT_ID,
        "action": "reject"
    });
    let reject = post_feedback(&pool, &session, reject_body).await;
    assert_eq!(reject.status(), StatusCode::CREATED);
    let reject_id = json_body(reject).await["product_match_id"]
        .as_str()
        .expect("product_match_id")
        .to_string();

    // The update reused the original row's id (NOT a delete-and-reinsert).
    assert_eq!(
        accept_id, reject_id,
        "upsert must update the existing row in place, preserving its id"
    );

    // Exactly one product_matches row, status rejected; two feedback rows.
    assert_eq!(match_row_count(&pool).await, 1);
    assert_eq!(match_status(&pool).await.as_deref(), Some("rejected"));
    assert_eq!(feedback_count(&pool).await, 2);

    // Both feedback rows point at the surviving match row (FK intact).
    let feedback_for_match: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM match_feedback WHERE product_match_id = ?1")
            .bind(&accept_id)
            .fetch_one(&pool)
            .await
            .expect("count feedback for match");
    assert_eq!(feedback_for_match, 2);
}

/// `prefer` maps the status to `accepted` while recording the verbatim `prefer`
/// action in `match_feedback` (history faithful to the user's choice).
#[tokio::test]
async fn prefer_maps_to_accepted_but_records_verbatim_action() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let body = serde_json::json!({
        "canonical_item_id": MILK_ID,
        "retailer_product_id": PRODUCT_ID,
        "action": "prefer"
    });
    let resp = post_feedback(&pool, &session, body).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let out = json_body(resp).await;
    assert_eq!(out["status"], "accepted");
    assert_eq!(out["action"], "prefer");

    assert_eq!(match_status(&pool).await.as_deref(), Some("accepted"));
    let recorded: String = sqlx::query_scalar("SELECT action FROM match_feedback")
        .fetch_one(&pool)
        .await
        .expect("recorded action");
    assert_eq!(recorded, "prefer");
}

/// An unknown `action` is 422, and writes nothing.
#[tokio::test]
async fn unknown_action_is_422() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let body = serde_json::json!({
        "canonical_item_id": MILK_ID,
        "retailer_product_id": PRODUCT_ID,
        "action": "explode"
    });
    let resp = post_feedback(&pool, &session, body).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(match_row_count(&pool).await, 0);
    assert_eq!(feedback_count(&pool).await, 0);
}

/// A caller who is NOT a member of the canonical item's household gets a 404 (the
/// item is treated as absent — never leaking another household's catalogue), and
/// nothing is written.
#[tokio::test]
async fn non_member_is_404() {
    let pool = setup_pool().await;
    // The item + product belong to HOUSEHOLD, but the caller belongs to OTHER.
    seed_household(&pool, HOUSEHOLD).await;
    seed_canonical_item(&pool, MILK_ID, HOUSEHOLD).await;
    seed_retailer_product(&pool, PRODUCT_ID).await;

    seed_household(&pool, OTHER_HOUSEHOLD).await;
    let session = seed_member_and_login(&pool, "user-b", "b@example.test", OTHER_HOUSEHOLD).await;

    let resp = post_feedback(&pool, &session, accept_body()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    assert_eq!(match_row_count(&pool).await, 0);
    assert_eq!(feedback_count(&pool).await, 0);
}

/// An unknown `canonical_item_id` is 404 (resolves no household).
#[tokio::test]
async fn unknown_canonical_item_is_404() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let body = serde_json::json!({
        "canonical_item_id": "ci-nope",
        "retailer_product_id": PRODUCT_ID,
        "action": "accept"
    });
    let resp = post_feedback(&pool, &session, body).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// An unknown `retailer_product_id` is 404 (the upsert would otherwise dangle a FK).
#[tokio::test]
async fn unknown_retailer_product_is_404() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let body = serde_json::json!({
        "canonical_item_id": MILK_ID,
        "retailer_product_id": "rp-nope",
        "action": "accept"
    });
    let resp = post_feedback(&pool, &session, body).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    assert_eq!(match_row_count(&pool).await, 0);
    assert_eq!(feedback_count(&pool).await, 0);
}

/// A POST without an `X-CSRF-Token` header is 403 (extractor-enforced), even with
/// a valid session cookie.
#[tokio::test]
async fn missing_csrf_is_403() {
    let pool = setup_pool().await;
    let session = seed_happy_path(&pool).await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/match-feedback")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                // Deliberately NO X-CSRF-Token header.
                .body(Body::from(accept_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// A POST without any session is 401 (extractor-enforced).
#[tokio::test]
async fn no_session_is_401() {
    let pool = setup_pool().await;
    let _ = seed_happy_path(&pool).await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/match-feedback")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(accept_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
