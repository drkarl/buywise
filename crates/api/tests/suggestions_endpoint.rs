//! Integration tests for the purchase-suggestion endpoint and its mutation
//! (issue #106).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied:
//!   * insufficient history (fewer than three purchase days) → no suggestion.
//!   * three weekly check-offs → the item is suggested, with its label.
//!   * an item already active on the list is never suggested.
//!   * `mark_bought` records purchase evidence; `move_to_list` removes it.
//!   * `dismiss_suggestion` records a dismissal and suppresses the next
//!     suggestion.
//!   * confirming a suggestion = `add_item` with `canonical_item_id`, which adds
//!     a resolved active entry and therefore removes the suggestion.
//!   * authorization: 404 for a missing list, 403 for a foreign household.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use chrono::{Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use serde_json::json;
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn seed_household(pool: &SqlitePool, id: &str, name: &str) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(id)
        .bind(name)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");
}

async fn seed_user(pool: &SqlitePool, id: &str, email: &str, household_id: &str) {
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(email)
    .bind(&hash)
    .bind(id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'owner', ?3)",
    )
    .bind(household_id)
    .bind(id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");
}

async fn seed_list(pool: &SqlitePool, id: &str, household_id: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'Groceries', 'active', 0, ?3)",
    )
    .bind(id)
    .bind(household_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Seed a category and a canonical item with an explicit English label.
async fn seed_item(pool: &SqlitePool, item_id: &str, household_id: &str, name_en: &str) {
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES ('cat-1', 'Dairy', 'Lácteos', 'milk', 1)",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES (?1, ?2, ?3, ?3, 'cat-1', 'any', ?4)",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(name_en)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");
}

/// Seed a check_off purchase evidence row `days_ago` days before today.
async fn seed_evidence(
    pool: &SqlitePool,
    id: &str,
    household_id: &str,
    item_id: &str,
    days_ago: i64,
) {
    let at = (Utc::now() - Duration::days(days_ago)).to_rfc3339_opts(SecondsFormat::Secs, true);
    sqlx::query(
        "INSERT INTO purchase_events \
             (id, household_id, canonical_item_id, list_id, entry_id, source, occurred_at, recorded_at) \
         VALUES (?1, ?2, ?3, NULL, NULL, 'check_off', ?4, ?4)",
    )
    .bind(id)
    .bind(household_id)
    .bind(item_id)
    .bind(&at)
    .execute(pool)
    .await
    .expect("insert purchase evidence");
}

/// Seed a RESOLVED active list entry bound to `item_id`.
async fn seed_resolved_entry(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    user_id: &str,
    item_id: &str,
) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'resolved', 'Milk', 'Milk', ?4, 1, 'active', 'default', 0, ?5, ?5)",
    )
    .bind(id)
    .bind(list_id)
    .bind(item_id)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert resolved entry");
}

async fn login(pool: &SqlitePool, email: &str) -> Session {
    let body = json!({ "email": email, "password": PASSWORD }).to_string();
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
    panic!("missing cookie {name}");
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// `GET /api/list/:id/suggestions` with the session cookie.
async fn get_suggestions(
    pool: &SqlitePool,
    s: &Session,
    list_id: &str,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/list/{list_id}/suggestions"))
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Build a `POST /api/mutations` envelope.
fn envelope(
    client_mutation_id: &str,
    list_id: &str,
    entity_type: &str,
    entity_id: &str,
    operation_type: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    json!({
        "client_mutation_id": client_mutation_id,
        "device_id": "test-device",
        "list_id": list_id,
        "base_revision": 0,
        "entity_type": entity_type,
        "entity_id": entity_id,
        "operation_type": operation_type,
        "payload": payload,
        "created_at_client": db::now_iso(),
    })
}

async fn post_mutation(
    pool: &SqlitePool,
    s: &Session,
    body: serde_json::Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mutations")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("x-csrf-token", &s.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Three weekly check-offs make the item due today.
async fn seed_weekly_history(pool: &SqlitePool, household_id: &str, item_id: &str) {
    seed_evidence(pool, "pe-1", household_id, item_id, 21).await;
    seed_evidence(pool, "pe-2", household_id, item_id, 14).await;
    seed_evidence(pool, "pe-3", household_id, item_id, 7).await;
}

#[tokio::test]
async fn insufficient_history_returns_no_suggestions() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_item(&pool, "item-milk", "hh-a", "Milk").await;
    seed_evidence(&pool, "pe-1", "hh-a", "item-milk", 14).await;
    seed_evidence(&pool, "pe-2", "hh-a", "item-milk", 7).await;

    let s = login(&pool, "a@example.test").await;
    let resp = get_suggestions(&pool, &s, "list-a").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json(resp).await;
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn regular_history_returns_the_due_item() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_item(&pool, "item-milk", "hh-a", "Milk").await;
    seed_weekly_history(&pool, "hh-a", "item-milk").await;

    let s = login(&pool, "a@example.test").await;
    let body = json(get_suggestions(&pool, &s, "list-a").await).await;
    let suggestions = body["suggestions"].as_array().unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0]["canonical_item_id"], "item-milk");
    assert_eq!(suggestions[0]["display_name"], "Milk");
    assert_eq!(suggestions[0]["typical_interval_days"], 7);
}

#[tokio::test]
async fn item_already_on_the_list_is_not_suggested() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_item(&pool, "item-milk", "hh-a", "Milk").await;
    seed_weekly_history(&pool, "hh-a", "item-milk").await;
    seed_resolved_entry(&pool, "entry-milk", "list-a", "user-a", "item-milk").await;

    let s = login(&pool, "a@example.test").await;
    let body = json(get_suggestions(&pool, &s, "list-a").await).await;
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn mark_bought_records_evidence_and_move_to_list_removes_it() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_item(&pool, "item-milk", "hh-a", "Milk").await;
    seed_resolved_entry(&pool, "entry-milk", "list-a", "user-a", "item-milk").await;
    let s = login(&pool, "a@example.test").await;

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "list_entry",
            "entry-milk",
            "mark_bought",
            json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM purchase_events WHERE entry_id = 'entry-milk'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "check-off should record purchase evidence");

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "list_entry",
            "entry-milk",
            "move_to_list",
            json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM purchase_events WHERE entry_id = 'entry-milk'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "undoing the check-off should remove its evidence");
}

#[tokio::test]
async fn dismissal_suppresses_the_next_suggestion() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_item(&pool, "item-milk", "hh-a", "Milk").await;
    seed_weekly_history(&pool, "hh-a", "item-milk").await;
    let s = login(&pool, "a@example.test").await;

    // Due before the dismissal.
    let body = json(get_suggestions(&pool, &s, "list-a").await).await;
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 1);

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-dismiss",
            "list-a",
            "canonical_item",
            "item-milk",
            "dismiss_suggestion",
            json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp_body = json(resp).await;
    assert!(resp_body["entity"].is_null(), "dismiss has no list entity");

    let dismissed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_dismissals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dismissed, 1);

    // Suppressed for one typical interval (7 days), so still absent right after.
    let body = json(get_suggestions(&pool, &s, "list-a").await).await;
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn confirming_with_add_item_removes_the_suggestion() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_item(&pool, "item-milk", "hh-a", "Milk").await;
    seed_weekly_history(&pool, "hh-a", "item-milk").await;
    let s = login(&pool, "a@example.test").await;

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-confirm",
            "list-a",
            "list_entry",
            "entry-new",
            "add_item",
            json!({ "canonical_item_id": "item-milk", "raw_text": "Milk", "display_name": "Milk", "quantity": 1 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = json(resp).await;
    assert_eq!(body["entity"]["resolution_status"], "resolved");
    assert_eq!(body["entity"]["canonical_item_id"], "item-milk");
    assert_eq!(body["entity"]["status"], "active");

    let body = json(get_suggestions(&pool, &s, "list-a").await).await;
    assert_eq!(body["suggestions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn suggestions_are_household_scoped() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-b", "b@example.test", "hh-b").await;

    let s = login(&pool, "b@example.test").await;
    let resp = get_suggestions(&pool, &s, "list-a").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = get_suggestions(&pool, &s, "list-missing").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dismissing_a_foreign_item_is_rejected() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_item(&pool, "item-b", "hh-b", "Foreign").await;

    let s = login(&pool, "a@example.test").await;
    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-foreign",
            "list-a",
            "canonical_item",
            "item-b",
            "dismiss_suggestion",
            json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
