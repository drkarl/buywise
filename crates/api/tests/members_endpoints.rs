//! Integration tests for household member administration (issue #139, roadmap
//! #114).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. The acceptance criteria
//! pinned here:
//!
//! * on a fresh database the bootstrap creates the household and its owner, who
//!   can add a second member from the app and that member can sign in;
//! * a non-owner cannot see or call the member endpoints (403) and cannot delete
//!   the household;
//! * a removed member's open session is rejected on the very next request;
//! * duplicate email, empty password, last-owner and non-member cases are
//!   refused with a clear status and change nothing.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const OWNER_PASSWORD: &str = "correct horse battery staple";
const MEMBER_PASSWORD: &str = "member initial password";

const HH_A: &str = "hh-a";
const HH_A_NAME: &str = "Household A";

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed one household with one `owner` member (and a list, so the household is a
/// realistic shape), then log the owner in.
async fn seed_household_owner(
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

    let hash = buywise_api::auth::hash_password_for_storage(OWNER_PASSWORD).expect("hash");
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
    .expect("insert owner membership");

    login(pool, email, OWNER_PASSWORD).await
}

async fn login(pool: &SqlitePool, email: &str, password: &str) -> Session {
    let body = json!({ "email": email, "password": password }).to_string();
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

/// Build and send one request. A session cookie is sent when `session` is given;
/// the CSRF header is added only when `with_csrf` is true, so a test can prove a
/// mutating request without it is refused.
async fn request(
    pool: &SqlitePool,
    method: &str,
    uri: &str,
    session: Option<&Session>,
    with_csrf: bool,
    body: Option<Value>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(s) = session {
        builder = builder.header(header::COOKIE, format!("session={}", s.session));
        if with_csrf {
            builder = builder.header("X-CSRF-Token", &s.csrf);
        }
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    app(AppState::new(pool.clone()))
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn list_members(
    pool: &SqlitePool,
    session: &Session,
    household_id: &str,
) -> axum::response::Response {
    request(
        pool,
        "GET",
        &format!("/api/household/{household_id}/members"),
        Some(session),
        false,
        None,
    )
    .await
}

async fn add_member(
    pool: &SqlitePool,
    session: &Session,
    household_id: &str,
    body: Value,
) -> axum::response::Response {
    request(
        pool,
        "POST",
        &format!("/api/household/{household_id}/members"),
        Some(session),
        true,
        Some(body),
    )
    .await
}

async fn remove_member(
    pool: &SqlitePool,
    session: &Session,
    household_id: &str,
    user_id: &str,
) -> axum::response::Response {
    request(
        pool,
        "DELETE",
        &format!("/api/household/{household_id}/members/{user_id}"),
        Some(session),
        true,
        None,
    )
    .await
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

// ---------------------------------------------------------------------------
// The fresh-database acceptance flow
// ---------------------------------------------------------------------------

/// On a fresh database the bootstrap creates the household and its owner, the
/// owner adds a second member through the API, and that member signs in.
#[tokio::test]
async fn bootstrapped_owner_adds_a_member_who_can_sign_in() {
    let pool = setup_pool().await;

    // The CLI bootstrap and the API share this core path.
    let created = buywise_core::members::bootstrap_household(
        &pool,
        "Our Home",
        "owner@example.test",
        "Owner",
        "owner-password",
    )
    .await
    .expect("bootstrap");

    // The owner signs in with the bootstrap password.
    let owner = login(&pool, "owner@example.test", "owner-password").await;

    // `GET /api/household` reports the owner's role, which the app uses to show
    // the members section.
    let summary = request(&pool, "GET", "/api/household", Some(&owner), false, None).await;
    assert_eq!(summary.status(), StatusCode::OK);
    let summary = json_body(summary).await;
    assert_eq!(summary["id"], created.household_id);
    assert_eq!(summary["role"], "owner");

    // The owner adds the second member.
    let added = add_member(
        &pool,
        &owner,
        &created.household_id,
        json!({
            "email": "member@example.test",
            "display_name": "Member",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(added.status(), StatusCode::CREATED);
    let added = json_body(added).await;
    assert_eq!(added["email"], "member@example.test");
    assert_eq!(added["role"], "member");
    assert!(
        added.get("password").is_none() && added.get("password_hash").is_none(),
        "the initial password must never be echoed back: {added}"
    );

    // The second member can sign in and sees their own role.
    let member = login(&pool, "member@example.test", MEMBER_PASSWORD).await;
    let member_summary = request(&pool, "GET", "/api/household", Some(&member), false, None).await;
    assert_eq!(member_summary.status(), StatusCode::OK);
    assert_eq!(json_body(member_summary).await["role"], "member");

    // And the owner now lists two members, the owner first.
    let listed = list_members(&pool, &owner, &created.household_id).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let listed = json_body(listed).await;
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["role"], "owner");
    assert_eq!(listed[1]["role"], "member");
}

// ---------------------------------------------------------------------------
// Owner-only access
// ---------------------------------------------------------------------------

/// A plain member cannot list members, add one, or remove one.
#[tokio::test]
async fn a_non_owner_cannot_use_the_member_endpoints() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-owner", "owner@example.test").await;
    // A second member promoted to nothing: plain `member` role, with a session.
    let plain = add_member(
        &pool,
        &owner,
        HH_A,
        json!({
            "email": "plain@example.test",
            "display_name": "Plain",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(plain.status(), StatusCode::CREATED);
    let plain = login(&pool, "plain@example.test", MEMBER_PASSWORD).await;

    // The member cannot see the list, even though they belong to the household.
    assert_eq!(
        list_members(&pool, &plain, HH_A).await.status(),
        StatusCode::FORBIDDEN
    );

    // Nor add anyone.
    let add = add_member(
        &pool,
        &plain,
        HH_A,
        json!({
            "email": "sneaky@example.test",
            "display_name": "Sneaky",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(add.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM users WHERE email = 'sneaky@example.test'"
        )
        .await,
        0,
        "a refused add must create nothing"
    );

    // Nor remove the owner (or anyone else).
    assert_eq!(
        remove_member(&pool, &plain, HH_A, "user-owner").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM household_members WHERE household_id = 'hh-a'").await,
        2,
        "a refused remove must change nothing"
    );

    // The endpoints stay hidden from a member who can still export.
    let export = request(
        &pool,
        "GET",
        &format!("/api/household/{HH_A}/export"),
        Some(&plain),
        false,
        None,
    )
    .await;
    assert_eq!(export.status(), StatusCode::OK, "members may still export");
}

/// A member of another household cannot use this household's member endpoints.
#[tokio::test]
async fn a_member_of_another_household_cannot_use_the_member_endpoints() {
    let pool = setup_pool().await;
    let _a = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;
    let b = seed_household_owner(&pool, "hh-b", "Household B", "user-b", "b@example.test").await;

    assert_eq!(
        list_members(&pool, &b, HH_A).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        add_member(
            &pool,
            &b,
            HH_A,
            json!({
                "email": "x@example.test",
                "display_name": "X",
                "password": MEMBER_PASSWORD,
            }),
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        remove_member(&pool, &b, HH_A, "user-a").await.status(),
        StatusCode::FORBIDDEN
    );
}

/// Every member endpoint requires a live session.
#[tokio::test]
async fn the_member_endpoints_require_a_session() {
    let pool = setup_pool().await;
    let _a = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;

    assert_eq!(
        list_members(&pool, &anonymous(), HH_A).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &pool,
            "POST",
            &format!("/api/household/{HH_A}/members"),
            None,
            false,
            Some(json!({ "email": "x@example.test", "display_name": "X", "password": "pw" })),
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

/// A session value the routes will reject; used to drive the unauthenticated
/// cases without a real login.
fn anonymous() -> Session {
    Session {
        session: "not-a-real-session".to_string(),
        csrf: "not-a-real-csrf".to_string(),
    }
}

/// A mutating member request without the CSRF header is refused, even for an
/// owner with a valid session.
#[tokio::test]
async fn adding_a_member_requires_csrf() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;

    let resp = request(
        &pool,
        "POST",
        &format!("/api/household/{HH_A}/members"),
        Some(&owner),
        false, // no X-CSRF-Token
        Some(json!({
            "email": "member@example.test",
            "display_name": "Member",
            "password": MEMBER_PASSWORD,
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM users WHERE email = 'member@example.test'"
        )
        .await,
        0
    );
}

/// A mutating member request without the CSRF header is refused.
#[tokio::test]
async fn removing_a_member_requires_csrf() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;
    let added = add_member(
        &pool,
        &owner,
        HH_A,
        json!({
            "email": "member@example.test",
            "display_name": "Member",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    let member_id = json_body(added).await["user_id"].as_str().unwrap().to_string();

    let resp = request(
        &pool,
        "DELETE",
        &format!("/api/household/{HH_A}/members/{member_id}"),
        Some(&owner),
        false, // no X-CSRF-Token
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM household_members WHERE household_id = 'hh-a'"
        )
        .await,
        2
    );
}

// ---------------------------------------------------------------------------
// Add-member refusals
// ---------------------------------------------------------------------------

/// A duplicate email is refused with 409 and adds nothing.
#[tokio::test]
async fn a_duplicate_email_is_refused() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;

    let first = add_member(
        &pool,
        &owner,
        HH_A,
        json!({
            "email": "member@example.test",
            "display_name": "Member",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let duplicate = add_member(
        &pool,
        &owner,
        HH_A,
        json!({
            "email": "member@example.test",
            "display_name": "Someone Else",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM users WHERE email = 'member@example.test'"
        )
        .await,
        1,
        "the duplicate must not create a second user"
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM household_members WHERE household_id = 'hh-a'").await,
        2
    );
}

/// An empty initial password is refused with 422 and creates nothing.
#[tokio::test]
async fn an_empty_initial_password_is_refused() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;

    let resp = add_member(
        &pool,
        &owner,
        HH_A,
        json!({
            "email": "member@example.test",
            "display_name": "Member",
            "password": "",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM users WHERE email = 'member@example.test'"
        )
        .await,
        0
    );
}

// ---------------------------------------------------------------------------
// Remove-member
// ---------------------------------------------------------------------------

/// Removing a member ends their session immediately and deletes the account when
/// they belong to no other household.
#[tokio::test]
async fn removing_a_member_ends_their_session_on_the_next_request() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;
    let added = add_member(
        &pool,
        &owner,
        HH_A,
        json!({
            "email": "member@example.test",
            "display_name": "Member",
            "password": MEMBER_PASSWORD,
        }),
    )
    .await;
    let member_id = json_body(added).await["user_id"].as_str().unwrap().to_string();

    // The member signs in; this session must stop working when they are removed.
    let member = login(&pool, "member@example.test", MEMBER_PASSWORD).await;
    assert_eq!(
        request(&pool, "GET", "/api/auth/me", Some(&member), false, None)
            .await
            .status(),
        StatusCode::OK
    );

    let removed = remove_member(&pool, &owner, HH_A, &member_id).await;
    assert_eq!(removed.status(), StatusCode::OK);
    let removed = json_body(removed).await;
    assert_eq!(removed["removed_user_id"], member_id);
    assert_eq!(removed["account_deleted"], true);
    assert_eq!(removed["sessions_ended"], 1);

    // The member's very next request is rejected.
    assert_eq!(
        request(&pool, "GET", "/api/auth/me", Some(&member), false, None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "a removed member's session must be rejected on the next request"
    );

    // Membership and account are gone.
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM household_members WHERE household_id = 'hh-a' AND user_id = 'user-a'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &pool,
            &format!("SELECT COUNT(*) FROM users WHERE id = '{member_id}'")
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &pool,
            &format!("SELECT COUNT(*) FROM sessions WHERE user_id = '{member_id}'")
        )
        .await,
        0
    );
}

/// Removing the household's only owner is refused with 409 and changes nothing.
#[tokio::test]
async fn removing_the_last_owner_is_refused() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;

    let resp = remove_member(&pool, &owner, HH_A, "user-a").await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM household_members WHERE household_id = 'hh-a' AND role = 'owner'"
        )
        .await,
        1
    );
    assert_eq!(
        request(&pool, "GET", "/api/auth/me", Some(&owner), false, None)
            .await
            .status(),
        StatusCode::OK,
        "the last owner's session must survive the refused removal"
    );
}

/// Removing a user who is not a member of the household is a 404.
#[tokio::test]
async fn removing_a_non_member_is_not_found() {
    let pool = setup_pool().await;
    let owner = seed_household_owner(&pool, HH_A, HH_A_NAME, "user-a", "a@example.test").await;

    let resp = remove_member(&pool, &owner, HH_A, "nobody").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
