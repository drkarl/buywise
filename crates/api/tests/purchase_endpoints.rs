//! Integration tests for the purchase-ledger endpoints (issue #148).
//!
//! Drives the real axum router against a private in-memory database with the
//! full migrations applied. Two households pin the acceptance criteria:
//! importing the same transaction twice yields one transaction and one set of
//! lines; a correction yields a new revision and keeps the earlier one; a member
//! of another household cannot read, import into or delete this household's
//! ledger; and the household export includes the imported records.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
const HH_A: &str = "hh-a";
const HH_B: &str = "hh-b";

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn seed_household(
    pool: &SqlitePool,
    household_id: &str,
    user_id: &str,
    email: &str,
) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?1, ?2)")
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
    panic!("cookie {name} not set");
}

/// A one-transaction import document with a described and a bare line.
fn document(source_transaction_id: Option<&str>, line_total: i64) -> Value {
    let mut transaction = json!({
        "retailer": "tesco",
        "purchase_date": "2026-05-01",
        "currency": "EUR",
        "total_cents": line_total + 100,
        "lines": [
            { "original_text": "Apples 1kg", "name": "Apples", "line_total_cents": line_total },
            { "original_text": "Mystery Item", "line_total_cents": 100 }
        ]
    });
    if let Some(id) = source_transaction_id {
        transaction["source_transaction_id"] = json!(id);
    }
    json!({ "format_version": 1, "transactions": [transaction] })
}

async fn post_import(
    pool: &SqlitePool,
    session: &Session,
    household_id: &str,
    body: &Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{household_id}/purchases"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get(pool: &SqlitePool, session: &Session, uri: &str) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete(pool: &SqlitePool, session: &Session, uri: &str) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
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

async fn seed_two(pool: &SqlitePool) -> (Session, Session) {
    let a = seed_household(pool, HH_A, "user-a", "a@example.test").await;
    let b = seed_household(pool, HH_B, "user-b", "b@example.test").await;
    (a, b)
}

#[tokio::test]
async fn import_then_list_and_get_returns_the_ledger() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;

    let resp = post_import(&pool, &a, HH_A, &document(Some("SRC-1"), 500)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let outcome = json_body(resp).await;
    assert_eq!(outcome["inserted"], 1);
    assert_eq!(outcome["results"][0]["revision"], 1);
    let transaction_id = outcome["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    let list = json_body(get(&pool, &a, &format!("/api/household/{HH_A}/purchases")).await).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], transaction_id.as_str());
    assert_eq!(list[0]["line_count"], 2);

    let detail = json_body(
        get(
            &pool,
            &a,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}"),
        )
        .await,
    )
    .await;
    assert_eq!(detail["lines"].as_array().unwrap().len(), 2);
    assert_eq!(detail["lines"][0]["original_text"], "Apples 1kg");
    assert_eq!(detail["lines"][1]["match_status"], "unmatched");
    assert!(detail["revisions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_reimport_is_unchanged_and_a_correction_keeps_the_earlier_revision() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;

    let first = json_body(post_import(&pool, &a, HH_A, &document(Some("SRC-1"), 500)).await).await;
    let transaction_id = first["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    let again = json_body(post_import(&pool, &a, HH_A, &document(Some("SRC-1"), 500)).await).await;
    assert_eq!(again["unchanged"], 1);
    assert_eq!(again["inserted"], 0);

    let corrected =
        json_body(post_import(&pool, &a, HH_A, &document(Some("SRC-1"), 600)).await).await;
    assert_eq!(corrected["updated"], 1);
    assert_eq!(corrected["results"][0]["revision"], 2);

    let detail = json_body(
        get(
            &pool,
            &a,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}"),
        )
        .await,
    )
    .await;
    assert_eq!(detail["revision"], 2);
    assert_eq!(detail["revisions"].as_array().unwrap().len(), 1);
    assert_eq!(detail["revisions"][0]["revision"], 1);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "a correction must not create a second transaction"
    );
}

#[tokio::test]
async fn a_member_of_another_household_is_refused() {
    let pool = setup_pool().await;
    let (_a, b) = seed_two(&pool).await;
    let tx = json_body(post_import(&pool, &b, HH_B, &document(Some("SRC-2"), 300)).await).await;
    let transaction_id = tx["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(
        post_import(&pool, &b, HH_A, &document(Some("SRC-1"), 500))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&pool, &b, &format!("/api/household/{HH_A}/purchases"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(
            &pool,
            &b,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete(
            &pool,
            &b,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    // Nothing was written into A, and B's own transaction is untouched.
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM purchase_transactions WHERE household_id = ?1")
            .bind(HH_A)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(total, 0);
}

#[tokio::test]
async fn a_transaction_of_another_household_reads_as_missing() {
    let pool = setup_pool().await;
    let (a, b) = seed_two(&pool).await;
    let tx = json_body(post_import(&pool, &b, HH_B, &document(Some("SRC-2"), 300)).await).await;
    let transaction_id = tx["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Member A asks for B's transaction id under A's own household path.
    assert_eq!(
        get(
            &pool,
            &a,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete(
            &pool,
            &a,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_malformed_import_reports_each_field() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;

    let bad = json!({
        "format_version": 1,
        "transactions": [{
            "retailer": "tesco",
            "purchase_date": "01-05-2026",
            "total_cents": 1.5,
            "extra": true,
            "lines": [{ "original_text": "x" }]
        }]
    });
    let resp = post_import(&pool, &a, HH_A, &bad).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(resp).await;
    let errors = body["errors"].to_string();
    assert!(
        errors.contains("transactions[0].extra: unknown field"),
        "{errors}"
    );
    assert!(
        errors.contains("transactions[0].lines[0].line_total_cents: missing required field"),
        "{errors}"
    );

    let unsupported = json!({ "format_version": 9, "transactions": [] });
    assert_eq!(
        post_import(&pool, &a, HH_A, &unsupported).await.status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn delete_removes_the_transaction() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;
    let tx = json_body(post_import(&pool, &a, HH_A, &document(Some("SRC-1"), 500)).await).await;
    let transaction_id = tx["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    let resp = delete(
        &pool,
        &a,
        &format!("/api/household/{HH_A}/purchases/{transaction_id}"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_body(resp).await["deleted"], true);

    assert_eq!(
        get(
            &pool,
            &a,
            &format!("/api/household/{HH_A}/purchases/{transaction_id}")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        json_body(get(&pool, &a, &format!("/api/household/{HH_A}/purchases")).await)
            .await
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn the_ledger_requires_an_authenticated_session() {
    let pool = setup_pool().await;
    let (_a, _b) = seed_two(&pool).await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{HH_A}/purchases"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_includes_imported_purchases() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;
    let tx = json_body(post_import(&pool, &a, HH_A, &document(Some("SRC-1"), 500)).await).await;
    let transaction_id = tx["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    let export = json_body(get(&pool, &a, &format!("/api/household/{HH_A}/export")).await).await;
    let transactions = export["purchase_transactions"].as_array().expect("array");
    assert_eq!(transactions.len(), 1);
    assert_eq!(transactions[0]["id"], transaction_id.as_str());
    assert_eq!(transactions[0]["lines"].as_array().unwrap().len(), 2);
}
