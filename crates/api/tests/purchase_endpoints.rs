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

// ---------------------------------------------------------------------------
// Manual purchases (issue #156)
// ---------------------------------------------------------------------------

/// Send a JSON body with a mutating method (POST/PUT) and the session's CSRF
/// token, on the same router the other helpers drive.
async fn send_json(
    pool: &SqlitePool,
    session: &Session,
    method: &str,
    uri: String,
    body: &Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// A manual-purchase body with two lines: milk at 258 and bread at 199 with a
/// 20-cent discount (line sum 437). `total` and `image` override the printed
/// total and the linked photo when supplied.
fn manual_body(retailer: &str, total: Option<i64>, image: Option<&str>) -> Value {
    let mut body = json!({
        "retailer": retailer,
        "purchase_date": "2026-10-01",
        "lines": [
            { "original_text": "Milk 1L", "quantity": 2, "line_total_cents": 258 },
            { "original_text": "Bread", "line_total_cents": 199, "line_discount_cents": 20 }
        ]
    });
    if let Some(total) = total {
        body["total_cents"] = json!(total);
    }
    if let Some(image) = image {
        body["receipt_image_id"] = json!(image);
    }
    body
}

/// A 32-hex-char storage key derived from `seed`, so the name passes the receipt
/// key shape check the delete path applies. No file is written: the delete
/// tolerates a missing file, which is exactly the outcome here.
fn fake_storage_key(seed: &str) -> String {
    let mut key: String = seed
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(32)
        .collect();
    while key.len() < 32 {
        key.push('0');
    }
    format!("{key}.jpg")
}

/// Insert one `receipt_images` row for a household. No bytes are written: the
/// tests that use it only need the record, and a delete tolerates a missing file.
async fn seed_receipt_image(pool: &SqlitePool, household_id: &str, id: &str, seed: &str) {
    sqlx::query(
        "INSERT INTO receipt_images \
         (id, household_id, uploaded_by, uploaded_at, expires_at, content_type, byte_size, \
          sha256_hex, storage_key, processing) \
         VALUES (?1, ?2, NULL, '2026-09-01T00:00:00.000Z', '2999-01-01T00:00:00.000Z', \
                 'image/jpeg', 4, 'aa', ?3, 'local')",
    )
    .bind(id)
    .bind(household_id)
    .bind(fake_storage_key(seed))
    .execute(pool)
    .await
    .expect("insert receipt image");
}

#[tokio::test]
async fn a_manual_purchase_is_created_listed_and_edited_as_one_revision() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;

    let resp = send_json(
        &pool,
        &a,
        "POST",
        format!("/api/household/{HH_A}/purchases/manual"),
        &manual_body("Tesco", None, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = json_body(resp).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["source_kind"], "manual");
    assert_eq!(created["retailer"], "tesco", "the retailer is normalised");
    assert_eq!(
        created["total_cents"], 437,
        "with no printed total the lines are the total"
    );
    assert_eq!(created["revision"], 1);
    assert_eq!(created["line_count"], 2);
    assert_eq!(created["lines"][0]["match_status"], "unmatched");
    assert_eq!(created["lines"][0]["quantity"], 2.0);
    assert!(created["lines"][0]["canonical_item_id"].is_null());
    assert!(created["receipt_image_id"].is_null());
    assert_eq!(created["identity_kind"], "source_id");
    let identity = created["source_transaction_id"]
        .as_str()
        .expect("a manual purchase has a generated identity");
    assert!(!identity.is_empty());

    // The detail carries the request body verbatim as the live source payload,
    // which is how a reader tells a printed total from one derived from the
    // lines (this body omitted `total_cents`).
    let payload: Value = serde_json::from_str(
        created["source_payload"]
            .as_str()
            .expect("the detail must expose the live source payload"),
    )
    .expect("the source payload is JSON");
    assert_eq!(payload["retailer"], "Tesco");
    assert!(
        payload.get("total_cents").is_none(),
        "the body left the printed total out: {payload}"
    );

    let list = json_body(get(&pool, &a, &format!("/api/household/{HH_A}/purchases")).await).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["line_count"], 2);

    // Edit the retailer, the date and the second line.
    let edit = json!({
        "retailer": "Lidl",
        "purchase_date": "2026-10-02",
        "total_cents": 500,
        "lines": [
            { "original_text": "Milk 1L", "quantity": 2, "line_total_cents": 258 },
            { "original_text": "Sourdough", "line_total_cents": 242 }
        ]
    });
    let edited = json_body(
        send_json(
            &pool,
            &a,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{id}"),
            &edit,
        )
        .await,
    )
    .await;
    assert_eq!(edited["id"], id.as_str());
    assert_eq!(edited["revision"], 2);
    assert_eq!(edited["retailer"], "lidl");
    assert_eq!(edited["total_cents"], 500);
    assert_eq!(edited["line_count"], 2);
    assert_eq!(edited["lines"][1]["original_text"], "Sourdough");
    assert_eq!(
        edited["source_transaction_id"], created["source_transaction_id"],
        "an edit keeps the purchase's identity"
    );
    let edited_payload: Value = serde_json::from_str(
        edited["source_payload"]
            .as_str()
            .expect("the corrected revision's payload is exposed"),
    )
    .expect("the source payload is JSON");
    assert_eq!(edited_payload["retailer"], "Lidl");
    assert_eq!(edited_payload["total_cents"], 500);
    assert_eq!(edited["revisions"].as_array().unwrap().len(), 1);
    assert_eq!(edited["revisions"][0]["revision"], 1);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM purchase_transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "an edit must not create a second purchase");

    // An edit with identical content is a no-op, not a third revision.
    let unchanged = json_body(
        send_json(
            &pool,
            &a,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{id}"),
            &edit,
        )
        .await,
    )
    .await;
    assert_eq!(unchanged["revision"], 2);
}

#[tokio::test]
async fn a_receipt_link_is_validated_and_clears_when_the_image_is_deleted() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;
    seed_receipt_image(&pool, HH_A, "ri-a", "aa").await;
    seed_receipt_image(&pool, HH_B, "ri-b", "bb").await;

    // Another household's photo is refused.
    let refused = send_json(
        &pool,
        &a,
        "POST",
        format!("/api/household/{HH_A}/purchases/manual"),
        &manual_body("tesco", Some(437), Some("ri-b")),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(json_body(refused).await["errors"]
        .to_string()
        .contains("receipt_image_id"));

    // The household's own photo links at create time.
    let created = json_body(
        send_json(
            &pool,
            &a,
            "POST",
            format!("/api/household/{HH_A}/purchases/manual"),
            &manual_body("tesco", Some(437), Some("ri-a")),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["receipt_image_id"], "ri-a");

    let export = json_body(get(&pool, &a, &format!("/api/household/{HH_A}/export")).await).await;
    assert_eq!(export["purchase_transactions"][0]["source_kind"], "manual");
    assert_eq!(
        export["purchase_transactions"][0]["receipt_image_id"],
        "ri-a"
    );
    assert_eq!(
        export["purchase_transactions"][0]["lines"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Deleting the image (the app's own delete, and what the expiry sweep does)
    // clears the link through the foreign key; the purchase and lines stay.
    let resp = delete(&pool, &a, &format!("/api/household/{HH_A}/receipts/ri-a")).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let after =
        json_body(get(&pool, &a, &format!("/api/household/{HH_A}/purchases/{id}")).await).await;
    assert!(
        after["receipt_image_id"].is_null(),
        "the link must clear with the image"
    );
    assert_eq!(after["id"], id.as_str());
    assert_eq!(after["line_count"], 2, "the purchase and its lines stay");
}

#[tokio::test]
async fn manual_purchases_are_private_and_imported_ones_cannot_be_edited_by_hand() {
    let pool = setup_pool().await;
    let (a, b) = seed_two(&pool).await;
    let created = json_body(
        send_json(
            &pool,
            &a,
            "POST",
            format!("/api/household/{HH_A}/purchases/manual"),
            &manual_body("tesco", None, None),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // A member of another household cannot edit it through A's path...
    assert_eq!(
        send_json(
            &pool,
            &b,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{id}"),
            &manual_body("lidl", None, None),
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send_json(
            &pool,
            &b,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{id}/receipt"),
            &json!({ "receipt_image_id": null }),
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    // ...and through their own path the id reads as missing.
    assert_eq!(
        send_json(
            &pool,
            &b,
            "PUT",
            format!("/api/household/{HH_B}/purchases/{id}"),
            &manual_body("lidl", None, None),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    // An imported purchase is corrected by re-importing, not by hand.
    let imported =
        json_body(post_import(&pool, &a, HH_A, &document(Some("SRC-9"), 500)).await).await;
    let imported_id = imported["results"][0]["transaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        send_json(
            &pool,
            &a,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{imported_id}"),
            &manual_body("tesco", None, None),
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // Its receipt link can still be set and cleared, without a revision.
    seed_receipt_image(&pool, HH_A, "ri-c", "cc").await;
    let linked = json_body(
        send_json(
            &pool,
            &a,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{imported_id}/receipt"),
            &json!({ "receipt_image_id": "ri-c" }),
        )
        .await,
    )
    .await;
    assert_eq!(linked["receipt_image_id"], "ri-c");
    assert_eq!(linked["revision"], 1, "linking a photo is not a revision");

    let cleared = json_body(
        send_json(
            &pool,
            &a,
            "PUT",
            format!("/api/household/{HH_A}/purchases/{imported_id}/receipt"),
            &json!({ "receipt_image_id": null }),
        )
        .await,
    )
    .await;
    assert!(cleared["receipt_image_id"].is_null());
}

#[tokio::test]
async fn manual_money_is_validated_and_a_negative_discount_is_allowed() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two(&pool).await;
    let uri = format!("/api/household/{HH_A}/purchases/manual");

    // A negative printed total, a negative line total and an unknown field are
    // each refused with the offending field named.
    let bad = json!({
        "retailer": "tesco",
        "purchase_date": "2026-10-01",
        "total_cents": -1,
        "surprise": true,
        "lines": [{ "original_text": "Milk", "line_total_cents": -5 }]
    });
    let resp = send_json(&pool, &a, "POST", uri.clone(), &bad).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let errors = json_body(resp).await["errors"].to_string();
    assert!(
        errors.contains("$.total_cents: must not be negative"),
        "{errors}"
    );
    assert!(
        errors.contains("lines[0].line_total_cents: must not be negative"),
        "{errors}"
    );
    assert!(errors.contains("$.surprise: unknown field"), "{errors}");

    // A negative discount (a surcharge) is the one place a negative amount is
    // accepted, and it raises the line sum.
    let created = json_body(
        send_json(
            &pool,
            &a,
            "POST",
            uri,
            &json!({
                "retailer": "spar",
                "purchase_date": "2026-10-01",
                "lines": [
                    { "original_text": "Milk", "line_total_cents": 258 },
                    { "original_text": "Bag charge", "line_total_cents": 199, "line_discount_cents": -50 }
                ]
            }),
        )
        .await,
    )
    .await;
    assert_eq!(created["total_cents"], 507);
    assert_eq!(created["lines"][1]["line_discount_cents"], -50);
}
