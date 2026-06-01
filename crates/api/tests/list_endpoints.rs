//! Integration tests for the P1-T3 list-management endpoints (issue #14).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied, covering every acceptance
//! criterion and the testing-expectation scenarios end-to-end:
//!   * create a list (POST /api/lists) then fetch it (GET /api/list/:id) and
//!     assert a top-level `current_revision == 0`.
//!   * GET /api/lists returns ONLY the caller's household's lists.
//!   * household isolation: an A-user reading B's list → 403 (not 404); a truly
//!     missing list id → 404.
//!   * GET /api/list/:id includes entries with `resolution_status` and
//!     `effective_constraint_hash`.
//!   * GET /api/items returns canonical items with aliases and constraints.
//!   * CSRF: a mutating call (POST/PATCH) without `X-CSRF-Token` → 403.
//!   * PATCH renames / archives a list.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";

/// A logged-in user's session + CSRF tokens, ready to attach to requests.
struct Session {
    session: String,
    csrf: String,
}

/// Build a fresh in-memory pool with migrations applied.
async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed a household.
async fn seed_household(pool: &SqlitePool, id: &str, name: &str) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?2, ?3)")
        .bind(id)
        .bind(name)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");
}

/// Seed a user with `PASSWORD` and add them as a member of `household_id`.
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

/// Seed a shopping list directly (used to set up the cross-household scenario).
async fn seed_list(pool: &SqlitePool, id: &str, household_id: &str, name: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists \
             (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, ?3, 'active', 0, ?4)",
    )
    .bind(id)
    .bind(household_id)
    .bind(name)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Seed one ACTIVE, unresolved list entry on a list.
async fn seed_entry(pool: &SqlitePool, id: &str, list_id: &str, added_by: &str, hash: &str) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, resolution_status, raw_text, display_name, added_by, quantity, \
              status, effective_constraint_hash, last_updated_revision, created_at, updated_at) \
         VALUES (?1, ?2, 'unresolved', ?3, ?3, ?4, 1, 'active', ?5, 0, ?6, ?6)",
    )
    .bind(id)
    .bind(list_id)
    .bind("Milk")
    .bind(added_by)
    .bind(hash)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert entry");
}

/// Seed a category and a canonical item belonging to `household_id`.
async fn seed_canonical_item(
    pool: &SqlitePool,
    category_id: &str,
    item_id: &str,
    household_id: &str,
) {
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES (?1, 'Dairy', 'Lácteos', 'milk', 1)",
    )
    .bind(category_id)
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
    .bind(category_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");
}

/// Seed one ACTIVE, resolved list entry bound to `canonical_item_id` with a
/// real (non-sentinel) `effective_constraint_hash`. Satisfies the resolution
/// CHECKs in migrations/003_phase1_schema.sql (resolved => canonical_item_id
/// NOT NULL and resolution_candidates_json NULL).
async fn seed_resolved_entry(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    added_by: &str,
    canonical_item_id: &str,
    hash: &str,
) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'resolved', ?4, ?4, ?5, 1, 'active', ?6, 0, ?7, ?7)",
    )
    .bind(id)
    .bind(list_id)
    .bind(canonical_item_id)
    .bind("Milk")
    .bind(added_by)
    .bind(hash)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert resolved entry");
}

/// Log in and return the session + csrf tokens.
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

/// Extract a named cookie value from the response's Set-Cookie headers.
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

/// Parse a response body as JSON.
async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_then_get_list_has_top_level_revision_zero() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    // POST /api/lists.
    let body = serde_json::json!({ "name": "Groceries", "household_id": "hh-a" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lists")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = json(resp).await;
    let list_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["current_revision"], 0);
    assert_eq!(created["name"], "Groceries");

    // GET /api/list/:id.
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri(format!("/api/list/{list_id}"))
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = json(resp).await;
    assert_eq!(
        detail["current_revision"], 0,
        "top-level revision must be 0"
    );
    assert_eq!(detail["id"], list_id);
    assert!(detail["entries"].is_array());
}

#[tokio::test]
async fn list_lists_returns_only_own_household() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_list(&pool, "list-b", "hh-b", "B's list").await;

    let s = login(&pool, "a@example.test").await;
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/lists")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let lists = json(resp).await;
    let arr = lists.as_array().unwrap();
    assert_eq!(arr.len(), 1, "only A's household list is visible");
    assert_eq!(arr[0]["id"], "list-a");
}

#[tokio::test]
async fn cross_household_get_is_403_and_missing_is_404() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-b", "hh-b", "B's list").await;

    let s = login(&pool, "a@example.test").await;

    // A-user reading B's (existing) list → 403, NOT 404.
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/list/list-b")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // A genuinely non-existent list id → 404.
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/list/does-not-exist")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_list_includes_entry_resolution_and_constraint_hash() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_entry(&pool, "entry-1", "list-a", "user-a", "unresolved:entry-1").await;

    let s = login(&pool, "a@example.test").await;
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/list/list-a")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = json(resp).await;
    let entries = detail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["resolution_status"], "unresolved");
    assert_eq!(
        entries[0]["effective_constraint_hash"],
        "unresolved:entry-1"
    );
    assert_eq!(entries[0]["id"], "entry-1");
}

#[tokio::test]
async fn create_list_in_foreign_household_is_403() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    let body = serde_json::json!({ "name": "Sneaky", "household_id": "hh-b" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lists")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn post_list_without_csrf_is_403() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    let body = serde_json::json!({ "name": "Groceries", "household_id": "hh-a" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/lists")
                .header(header::COOKIE, format!("session={}", s.session))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unauthenticated_list_request_is_401() {
    let pool = setup_pool().await;
    let resp = app(AppState::new(pool))
        .oneshot(
            Request::builder()
                .uri("/api/lists")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn patch_renames_and_archives_list() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "Old name").await;
    let s = login(&pool, "a@example.test").await;

    // Rename.
    let body = serde_json::json!({ "name": "New name" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/list/list-a")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = json(resp).await;
    assert_eq!(detail["name"], "New name");
    assert_eq!(detail["status"], "active");

    // Archive.
    let body = serde_json::json!({ "status": "archived" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/list/list-a")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = json(resp).await;
    assert_eq!(detail["status"], "archived");
    assert!(detail["archived_at"].is_string(), "archived_at is set");

    // Archived list no longer appears in GET /api/lists.
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/lists")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let lists = json(resp).await;
    assert_eq!(
        lists.as_array().unwrap().len(),
        0,
        "archived list is hidden"
    );
}

#[tokio::test]
async fn patch_foreign_list_is_403() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-b", "hh-b", "B's list").await;
    let s = login(&pool, "a@example.test").await;

    let body = serde_json::json!({ "name": "Hijacked" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/list/list-b")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn patch_with_invalid_status_is_422() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    let body = serde_json::json!({ "status": "deleted" }).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/list/list-a")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn patch_with_empty_body_is_422() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    // An empty object changes nothing → 422 (no-op body is rejected).
    let body = serde_json::json!({}).to_string();
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/list/list-a")
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn get_list_surfaces_resolved_entry_canonical_id_and_hash() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_canonical_item(&pool, "cat-1", "item-1", "hh-a").await;
    seed_resolved_entry(
        &pool,
        "entry-1",
        "list-a",
        "user-a",
        "item-1",
        "hash-abc123",
    )
    .await;

    let s = login(&pool, "a@example.test").await;
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/list/list-a")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail = json(resp).await;
    let entries = detail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["resolution_status"], "resolved");
    assert_eq!(entries[0]["canonical_item_id"], "item-1");
    assert_eq!(entries[0]["effective_constraint_hash"], "hash-abc123");
}

#[tokio::test]
async fn get_items_returns_items_with_aliases_and_constraints() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;

    // A category (canonical_items.category_id references categories).
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES ('cat-1', 'Dairy', 'Lácteos', 'milk', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES ('item-1', 'hh-a', 'Milk', 'Leche', 'cat-1', 'any', ?1)",
    )
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO item_aliases \
             (id, household_id, canonical_item_id, alias, language, source, created_at) \
         VALUES ('alias-1', 'hh-a', 'item-1', 'leche', 'es', 'manual', ?1)",
    )
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO item_constraints \
             (id, canonical_item_id, brand, substitution_policy, updated_at) \
         VALUES ('con-1', 'item-1', 'Avonmore', 'any', ?1)",
    )
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .unwrap();

    let s = login(&pool, "a@example.test").await;
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .uri("/api/items")
                .header(header::COOKIE, format!("session={}", s.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let items = json(resp).await;
    let arr = items.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "item-1");
    assert_eq!(arr[0]["aliases"].as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["aliases"][0]["alias"], "leche");
    assert_eq!(arr[0]["constraints"]["brand"], "Avonmore");
}
