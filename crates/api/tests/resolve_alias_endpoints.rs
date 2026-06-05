//! Integration tests for the `resolve_alias` mutation (P3-T3, issue #27).
//!
//! The user-confirmation step of the resolution flow: an `ambiguous` list entry
//! is bound to the canonical item the user chose, transitioning it to `resolved`,
//! and the confirmed `raw_text` → canonical mapping is persisted as an
//! `item_aliases` row with `source='accepted_match'`. Both writes must land in
//! one transaction (atomic bind + alias). Drives the real axum router against an
//! in-memory SQLite database with the full migrations applied.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
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

async fn seed_category(pool: &SqlitePool, id: &str) {
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES (?1, 'Dairy', 'Lácteos', 'milk', 1)",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("insert category");
}

/// Seed one canonical item owned by `household_id`.
async fn seed_canonical_item(
    pool: &SqlitePool,
    item_id: &str,
    name_en: &str,
    household_id: &str,
    category_id: &str,
) {
    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES (?1, ?2, ?3, ?3, ?4, 'any', ?5)",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(name_en)
    .bind(category_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");
}

/// Seed an `ambiguous` list entry carrying `candidates` as its candidate JSON.
async fn seed_ambiguous_entry(
    pool: &SqlitePool,
    entry_id: &str,
    list_id: &str,
    user_id: &str,
    raw_text: &str,
    candidates: &[&str],
) {
    let candidates_json = serde_json::json!(candidates).to_string();
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, resolution_candidates_json, \
              raw_text, display_name, added_by, quantity, status, effective_constraint_hash, \
              last_updated_revision, created_at, updated_at) \
         VALUES (?1, ?2, NULL, 'ambiguous', ?3, ?4, ?4, ?5, 1, 'active', ?6, 0, ?7, ?7)",
    )
    .bind(entry_id)
    .bind(list_id)
    .bind(&candidates_json)
    .bind(raw_text)
    .bind(user_id)
    .bind(format!("unresolved:{entry_id}"))
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert ambiguous entry");
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
    panic!("missing cookie {name}");
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
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
                .header(header::COOKIE, format!("session={}", s.session))
                .header("X-CSRF-Token", &s.csrf)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn resolve_alias_envelope(
    client_mutation_id: &str,
    list_id: &str,
    entry_id: &str,
    chosen_canonical_item_id: &str,
    raw_text: &str,
) -> serde_json::Value {
    serde_json::json!({
        "client_mutation_id": client_mutation_id,
        "device_id": "device-1",
        "list_id": list_id,
        "base_revision": 0,
        "entity_type": "list_entry",
        "entity_id": entry_id,
        "operation_type": "resolve_alias",
        "payload": {
            "chosen_canonical_item_id": chosen_canonical_item_id,
            "raw_text": raw_text,
        },
        "created_at_client": "2026-06-05T00:00:00.000Z",
    })
}

/// Seed a household with two cream candidates and an ambiguous "cream" entry.
async fn seed_cream_scenario(pool: &SqlitePool) -> Session {
    seed_household(pool, "hh-a", "Household A").await;
    seed_user(pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(pool, "list-a", "hh-a", "A's list").await;
    seed_category(pool, "cat-dairy").await;
    seed_canonical_item(pool, "ci-single", "Single cream", "hh-a", "cat-dairy").await;
    seed_canonical_item(pool, "ci-double", "Double cream", "hh-a", "cat-dairy").await;
    seed_ambiguous_entry(
        pool,
        "entry-1",
        "list-a",
        "user-a",
        "cream",
        &["ci-single", "ci-double"],
    )
    .await;
    login(pool, "a@example.test").await
}

// ---------------------------------------------------------------------------
// Acceptance criteria
// ---------------------------------------------------------------------------

/// Confirming a candidate transitions the entry to `resolved`, binds the chosen
/// canonical item, clears the candidates, and persists an `accepted_match` alias.
#[tokio::test]
async fn resolve_alias_resolves_entry_and_writes_accepted_match_alias() {
    let pool = setup_pool().await;
    let s = seed_cream_scenario(&pool).await;

    let resp = post_mutation(
        &pool,
        &s,
        resolve_alias_envelope("cm-1", "list-a", "entry-1", "ci-double", "cream"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = json(resp).await;
    assert_eq!(v["status"], "accepted");
    assert_eq!(v["entity"]["resolution_status"], "resolved");
    assert_eq!(v["entity"]["canonical_item_id"], "ci-double");
    assert!(
        v["entity"]["resolution_candidates_json"].is_null(),
        "resolved entry clears its candidates"
    );

    // The entry row reflects the bind.
    let (status, canonical): (String, Option<String>) = sqlx::query_as(
        "SELECT resolution_status, canonical_item_id FROM list_entries WHERE id = 'entry-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "resolved");
    assert_eq!(canonical.as_deref(), Some("ci-double"));

    // Exactly one alias row, normalised, with source='accepted_match'.
    let aliases: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT alias, canonical_item_id, source FROM item_aliases WHERE household_id = 'hh-a'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(aliases.len(), 1, "exactly one alias persisted");
    assert_eq!(aliases[0].0, "cream");
    assert_eq!(aliases[0].1, "ci-double");
    assert_eq!(aliases[0].2, "accepted_match");
}

/// Confirming the same term twice is idempotent at the alias level: the
/// `client_mutation_id` replay short-circuits, and even a fresh confirm hitting
/// the already-resolved entry does not write a second alias (unique index).
#[tokio::test]
async fn confirming_twice_writes_a_single_alias() {
    let pool = setup_pool().await;
    let s = seed_cream_scenario(&pool).await;

    let first = post_mutation(
        &pool,
        &s,
        resolve_alias_envelope("cm-1", "list-a", "entry-1", "ci-double", "cream"),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    // A second confirm with a DIFFERENT client_mutation_id reaches the apply path
    // again, but the entry is no longer `ambiguous` → guarded no-op accept, and the
    // alias unique index keeps the table at one row.
    let second = post_mutation(
        &pool,
        &s,
        resolve_alias_envelope("cm-2", "list-a", "entry-1", "ci-double", "cream"),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CREATED);
    let v = json(second).await;
    assert_eq!(v["status"], "accepted");
    assert_eq!(v["entity"]["resolution_status"], "resolved");

    let alias_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases WHERE household_id = 'hh-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(alias_count, 1, "no duplicate alias on a repeat confirm");
}

/// Binding a canonical item from another household is rejected (404) and writes
/// nothing — the cross-household catalogue guard.
#[tokio::test]
async fn resolve_alias_rejects_foreign_canonical_item() {
    let pool = setup_pool().await;
    let s = seed_cream_scenario(&pool).await;
    // A second household with its own item the caller must NOT be able to bind.
    seed_household(&pool, "hh-b", "Household B").await;
    seed_canonical_item(&pool, "ci-foreign", "Sour cream", "hh-b", "cat-dairy").await;

    let resp = post_mutation(
        &pool,
        &s,
        resolve_alias_envelope("cm-1", "list-a", "entry-1", "ci-foreign", "cream"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Entry untouched (still ambiguous) and no alias written.
    let status: String =
        sqlx::query_scalar("SELECT resolution_status FROM list_entries WHERE id = 'entry-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "ambiguous");
    // Scoped to the test households (the migration seed-data populates
    // `item_aliases` for its own seed household, which is unrelated here).
    let alias_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM item_aliases WHERE household_id IN ('hh-a', 'hh-b')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(alias_count, 0, "rejected bind writes no alias");
}

/// A missing `chosen_canonical_item_id` is a 422 at the boundary.
#[tokio::test]
async fn resolve_alias_without_chosen_item_is_422() {
    let pool = setup_pool().await;
    let s = seed_cream_scenario(&pool).await;

    let mut body = resolve_alias_envelope("cm-1", "list-a", "entry-1", "ci-double", "cream");
    body["payload"] = serde_json::json!({ "raw_text": "cream" });

    let resp = post_mutation(&pool, &s, body).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
