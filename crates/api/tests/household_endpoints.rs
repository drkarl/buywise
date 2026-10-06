//! Integration tests for the member-facing household data controls (issue #136).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Two seeded households pin
//! the two acceptance criteria:
//!
//! * export returns the documented JSON layout for the caller's household only,
//!   and a member of a different household is refused;
//! * delete requires the exact household name and the member's own password,
//!   removes only the target household's rows, ends the caller's session, and
//!   leaves the other household byte-for-byte unchanged.

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
const HH_A_NAME: &str = "Household A";
const HH_B_NAME: &str = "Household B";

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed one household with one member and one shopping list, then log the
/// member in. `role` lets a test seed a shared (non-owner) member.
async fn seed_household(
    pool: &SqlitePool,
    household_id: &str,
    household_name: &str,
    user_id: &str,
    email: &str,
) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(household_id)
        .bind(household_name)
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

    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(format!("list-{household_id}"))
    .bind(household_id)
    .bind(format!("{household_name} list"))
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");

    login(pool, email).await
}

/// Add `user_id` as a member of an existing household (a shared member).
async fn add_membership(pool: &SqlitePool, household_id: &str, user_id: &str) {
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'member', ?3)",
    )
    .bind(household_id)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");
}

/// Insert a user with the test password and a plain `member` membership of
/// `household_id`, then log them in.
async fn seed_plain_member(
    pool: &SqlitePool,
    household_id: &str,
    user_id: &str,
    email: &str,
) -> Session {
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(user_id)
    .bind(email)
    .bind(&hash)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");
    add_membership(pool, household_id, user_id).await;
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

fn has_cookie(resp: &axum::response::Response, name: &str) -> bool {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|value| {
            let s = value.to_str().unwrap();
            let first = s.split(';').next().unwrap();
            first
                .split_once('=')
                .map(|(k, _)| k.trim() == name)
                .unwrap_or(false)
        })
}

async fn get_household(pool: &SqlitePool, session: &Session) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/household")
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn export(
    pool: &SqlitePool,
    session: &Session,
    household_id: &str,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{household_id}/export"))
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn delete(
    pool: &SqlitePool,
    session: &Session,
    household_id: &str,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{household_id}/delete"))
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

async fn scalar(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("scalar")
}

/// Two households, A and B, each with a member and a list.
async fn seed_two_households(pool: &SqlitePool) -> (Session, Session) {
    let a = seed_household(pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;
    let b = seed_household(pool, HH_B, HH_B_NAME, "user-b", "b@example.test").await;
    (a, b)
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// The export for the caller's household carries the documented envelope and
/// that household's own records, and nothing from the other household.
#[tokio::test]
async fn export_returns_the_documented_layout_for_the_callers_household() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two_households(&pool).await;

    // `GET /api/household` gives the app the id and name it needs.
    let summary = get_household(&pool, &a).await;
    assert_eq!(summary.status(), StatusCode::OK);
    let summary = json_body(summary).await;
    assert_eq!(summary["id"], HH_A);
    assert_eq!(summary["name"], HH_A_NAME);

    let resp = export(&pool, &a, HH_A).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap(),
        "attachment; filename=\"buywise-household-hh-a.json\""
    );

    let doc = json_body(resp).await;
    assert_eq!(doc["format"], "buywise-household-export");
    assert_eq!(doc["format_version"], 1);
    assert_eq!(doc["household"]["id"], HH_A);
    assert_eq!(doc["household"]["name"], HH_A_NAME);
    assert_eq!(doc["members"][0]["user_id"], "user-a");
    assert_eq!(doc["lists"][0]["name"], "Household A list");

    // The other household must not leak into the document.
    let text = doc.to_string();
    assert!(!text.contains(HH_B), "household B leaked into A's export");
    assert!(
        !text.contains("Household B"),
        "household B's list leaked into A's export"
    );
}

/// A member of another household cannot export this one.
#[tokio::test]
async fn export_refuses_a_member_of_another_household() {
    let pool = setup_pool().await;
    let (_a, b) = seed_two_households(&pool).await;

    let resp = export(&pool, &b, HH_A).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Export without a session is 401.
#[tokio::test]
async fn export_requires_an_authenticated_session() {
    let pool = setup_pool().await;
    let _ = seed_two_households(&pool).await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/household/{HH_A}/export"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A caller who belongs to no household has no data controls.
#[tokio::test]
async fn get_household_is_not_found_without_a_membership() {
    let pool = setup_pool().await;
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES ('u-none', 'none@example.test', ?1, 'None', ?2)",
    )
    .bind(&hash)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert user");
    let session = login(&pool, "none@example.test").await;

    assert_eq!(
        get_household(&pool, &session).await.status(),
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// The wrong confirmation name is refused and nothing changes.
#[tokio::test]
async fn delete_with_a_wrong_confirmation_changes_nothing() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two_households(&pool).await;

    let resp = delete(
        &pool,
        &a,
        HH_A,
        json!({ "household_name": "Household Z", "password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM households WHERE id = 'hh-a'").await,
        1
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM shopping_lists WHERE household_id = 'hh-a'"
        )
        .await,
        1
    );
}

/// The wrong password is refused and nothing changes.
#[tokio::test]
async fn delete_with_a_wrong_password_changes_nothing() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two_households(&pool).await;

    let resp = delete(
        &pool,
        &a,
        HH_A,
        json!({ "household_name": HH_A_NAME, "password": "not the password" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM households WHERE id = 'hh-a'").await,
        1
    );
}

/// A member of another household cannot delete this one.
#[tokio::test]
async fn delete_refuses_a_member_of_another_household() {
    let pool = setup_pool().await;
    let (_a, b) = seed_two_households(&pool).await;

    let resp = delete(
        &pool,
        &b,
        HH_A,
        json!({ "household_name": HH_A_NAME, "password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM households WHERE id = 'hh-a'").await,
        1
    );
}

/// A plain (non-owner) member cannot delete the household, even with the exact
/// name and their correct password: the delete action is owners-only.
#[tokio::test]
async fn delete_refuses_a_non_owner_member() {
    let pool = setup_pool().await;
    let _a = seed_household(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;
    let plain = seed_plain_member(&pool, HH_A, "user-plain", "plain@example.test").await;

    let resp = delete(
        &pool,
        &plain,
        HH_A,
        json!({ "household_name": HH_A_NAME, "password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Nothing was removed and the member's own session still works.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM households WHERE id = 'hh-a'").await,
        1
    );
    assert_eq!(
        get_household(&pool, &plain).await.status(),
        StatusCode::OK,
        "a refused delete must not end the member's session"
    );
}

/// A delete without the CSRF header is refused, even with a valid session.
#[tokio::test]
async fn delete_without_csrf_is_forbidden() {
    let pool = setup_pool().await;
    let (a, _b) = seed_two_households(&pool).await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/household/{HH_A}/delete"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", a.session))
                // Deliberately no X-CSRF-Token header.
                .body(Body::from(
                    json!({ "household_name": HH_A_NAME, "password": PASSWORD }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Correct confirmation removes only the target household and ends the session.
#[tokio::test]
async fn delete_with_correct_input_removes_only_the_target_household() {
    let pool = setup_pool().await;
    let (a, b) = seed_two_households(&pool).await;

    let resp = delete(
        &pool,
        &a,
        HH_A,
        json!({ "household_name": HH_A_NAME, "password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        has_cookie(&resp, "session"),
        "session cookie must be cleared"
    );
    assert!(has_cookie(&resp, "csrf"), "csrf cookie must be cleared");
    let body = json_body(resp).await;
    assert!(body["removed_rows"].as_u64().unwrap() >= 2);

    // A is gone, including the member (orphaned), their list and their session.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM households WHERE id = 'hh-a'").await,
        0
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM shopping_lists WHERE household_id = 'hh-a'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM users WHERE id = 'user-a'").await,
        0
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sessions WHERE user_id = 'user-a'"
        )
        .await,
        0
    );

    // B is untouched, and B's session still works.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM households WHERE id = 'hh-b'").await,
        1
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM shopping_lists WHERE household_id = 'hh-b'"
        )
        .await,
        1
    );
    assert_eq!(export(&pool, &b, HH_B).await.status(), StatusCode::OK);
}

/// A member who also belongs to another household keeps their account, but their
/// session is still ended by the delete.
#[tokio::test]
async fn delete_ends_a_shared_members_session_but_keeps_their_other_membership() {
    let pool = setup_pool().await;
    // `user-shared` belongs to both households; deleting A must keep the user
    // (still a member of B) while ending the session they deleted from. Seeding
    // A logs them in once, which is the session under test.
    let shared = seed_household(&pool, HH_A, HH_A_NAME, "user-shared", "shared@example.test").await;
    let _b = seed_household(&pool, HH_B, HH_B_NAME, "user-b", "b@example.test").await;
    add_membership(&pool, HH_B, "user-shared").await;

    let resp = delete(
        &pool,
        &shared,
        HH_A,
        json!({ "household_name": HH_A_NAME, "password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM users WHERE id = 'user-shared'").await,
        1,
        "a user in another household must survive"
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM household_members WHERE user_id = 'user-shared' \
             AND household_id = 'hh-b'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sessions WHERE user_id = 'user-shared'"
        )
        .await,
        0,
        "the deleting member's session must end"
    );
}
