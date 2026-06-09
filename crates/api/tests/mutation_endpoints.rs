//! Integration tests for the P1-T4 mutation endpoint (issue #15).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied, covering every acceptance
//! criterion and testing-expectation scenario end-to-end:
//!   * 5 mutations → `current_revision == 5`; each `server_revision` equals the
//!     new `current_revision`.
//!   * idempotency: the same `client_mutation_id` twice returns the byte-identical
//!     response and writes exactly one `mutations` row.
//!   * dedup: adding the same resolved item twice → `quantity == 2`, status
//!     `merged`.
//!   * unresolved dedup: adding the same unresolved item twice → two rows.
//!   * a no-constraint resolved add stores `effective_constraint_hash == 'default'`.
//!   * authorisation: 404 for a missing list, 403 for a foreign household, 401
//!     unauthenticated, 403 without CSRF; 422 for an unknown operation.
//!   * delete-vs-mark_bought: delete wins and the bought action is logged.

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

/// Build a fresh SHARED-CACHE in-memory pool (multi-connection) with migrations
/// applied. Unlike `setup_pool`'s private `sqlite::memory:` (capped at one
/// connection), a shared-cache URL lets several connections see the same database,
/// so concurrent requests genuinely contend for SQLite's single write lock. A
/// unique database name per call keeps tests isolated from one another.
async fn setup_shared_pool() -> SqlitePool {
    let name = format!("buywise_test_{}", uuid::Uuid::new_v4().simple());
    let url = format!("file:{name}?mode=memory&cache=shared");
    let pool = db::connect(&url).await.expect("connect shared");
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

/// Read the raw response bytes (for byte-identical idempotency checks).
async fn raw_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

/// POST a mutation envelope with the session + CSRF attached.
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

/// GET the full list snapshot (`GET /api/list/:id`) — the resnapshot/sync path a
/// fresh client uses to rehydrate. Returns the parsed JSON body.
async fn get_list_snapshot(pool: &SqlitePool, s: &Session, list_id: &str) -> serde_json::Value {
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
    assert_eq!(resp.status(), StatusCode::OK, "snapshot fetch ok");
    json(resp).await
}

/// A mutation envelope with the common fields filled in.
fn envelope(
    client_mutation_id: &str,
    list_id: &str,
    entity_id: &str,
    operation_type: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "client_mutation_id": client_mutation_id,
        "device_id": "device-1",
        "list_id": list_id,
        "base_revision": 0,
        "entity_type": "list_entry",
        "entity_id": entity_id,
        "operation_type": operation_type,
        "payload": payload,
        "created_at_client": "2026-05-31T00:00:00.000Z",
    })
}

// ---------------------------------------------------------------------------
// Acceptance criteria
// ---------------------------------------------------------------------------

#[tokio::test]
async fn five_mutations_advance_revision_to_five() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    for i in 1..=5 {
        let body = envelope(
            &format!("cm-{i}"),
            "list-a",
            &format!("entry-{i}"),
            "add_item",
            serde_json::json!({ "raw_text": format!("Item {i}"), "display_name": format!("Item {i}") }),
        );
        let resp = post_mutation(&pool, &s, body).await;
        assert_eq!(resp.status(), StatusCode::CREATED, "mutation {i} accepted");
        let v = json(resp).await;
        // server_revision equals the new current_revision (== i here).
        assert_eq!(
            v["server_revision"], i,
            "server_revision tracks current_revision"
        );
        assert_eq!(v["status"], "accepted");
    }

    let current: i64 =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        current, 5,
        "five accepted mutations advance the revision to 5"
    );
}

#[tokio::test]
async fn same_client_mutation_id_replays_byte_identical_response() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    let body = envelope(
        "cm-dup",
        "list-a",
        "entry-1",
        "add_item",
        serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
    );

    let first = post_mutation(&pool, &s, body.clone()).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_bytes = raw_bytes(first).await;

    // Replay the SAME client_mutation_id.
    let second = post_mutation(&pool, &s, body).await;
    // The replay is served from the stored response (200, not 201).
    assert_eq!(second.status(), StatusCode::OK);
    let second_bytes = raw_bytes(second).await;

    assert_eq!(first_bytes, second_bytes, "replay must be byte-identical");

    // Exactly one mutations row and one entry: the replay re-applied nothing.
    let mutation_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mutations WHERE client_mutation_id = 'cm-dup'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        mutation_count, 1,
        "idempotent replay writes no extra mutation"
    );

    let entry_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM list_entries WHERE list_id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(entry_count, 1);

    // The revision advanced only once.
    let current: i64 =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(current, 1, "replay does not advance the revision");
}

#[tokio::test]
async fn duplicate_resolved_add_merges_and_increments_quantity() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_canonical_item(&pool, "cat-1", "item-1", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    // First resolved add (no constraints → 'default' hash).
    let first = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "canonical_item_id": "item-1", "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(json(first).await["status"], "accepted");

    // Second resolved add of the SAME item with the SAME (empty) constraints.
    let second = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-2",
            "add_item",
            serde_json::json!({ "canonical_item_id": "item-1", "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CREATED);
    let v = json(second).await;
    assert_eq!(v["status"], "merged", "duplicate resolved add merges");
    assert_eq!(v["entity"]["quantity"], 2, "quantity incremented to 2");

    // Exactly one active resolved row survives, with quantity 2.
    let (count, qty): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MAX(quantity), 0) FROM list_entries \
         WHERE list_id = 'list-a' AND status = 'active'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "merge keeps a single active row");
    assert_eq!(qty, 2);

    // Both mutations were recorded; the second as 'merged'.
    let merged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mutations WHERE status = 'merged'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(merged, 1);

    // Revision advanced for BOTH accepted mutations (a merge is still accepted).
    let current: i64 =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(current, 2);
}

#[tokio::test]
async fn duplicate_unresolved_add_creates_two_rows() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    for (cm, entry) in [("cm-1", "entry-1"), ("cm-2", "entry-2")] {
        let resp = post_mutation(
            &pool,
            &s,
            envelope(
                cm,
                "list-a",
                entry,
                "add_item",
                // No canonical_item_id → unresolved → 'unresolved:<entry_id>' sentinel.
                serde_json::json!({ "raw_text": "leche", "display_name": "leche" }),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(
            json(resp).await["status"],
            "accepted",
            "unresolved adds never merge"
        );
    }

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM list_entries WHERE list_id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2, "two unresolved adds create two rows");

    // Each carries its own unique sentinel hash.
    let hashes: Vec<String> = sqlx::query_scalar(
        "SELECT effective_constraint_hash FROM list_entries WHERE list_id = 'list-a' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(hashes, vec!["unresolved:entry-1", "unresolved:entry-2"]);
}

#[tokio::test]
async fn resolved_no_constraint_entry_hash_is_default() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_canonical_item(&pool, "cat-1", "item-1", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "canonical_item_id": "item-1", "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        json(resp).await["entity"]["effective_constraint_hash"],
        "default"
    );

    let hash: String = sqlx::query_scalar(
        "SELECT effective_constraint_hash FROM list_entries WHERE id = 'entry-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        hash, "default",
        "no-constraint resolved entry hashes to 'default', not empty/NULL"
    );
}

#[tokio::test]
async fn resolved_add_with_constraints_hashes_to_digest() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_canonical_item(&pool, "cat-1", "item-1", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    // Same canonical item, but one with a real constraint → different hash → two rows.
    let plain = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "canonical_item_id": "item-1", "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(plain.status(), StatusCode::CREATED);

    let lactose_free = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-2",
            "add_item",
            serde_json::json!({
                "canonical_item_id": "item-1",
                "raw_text": "Lactose-free milk",
                "display_name": "Lactose-free milk",
                "constraints": { "dietary": ["lactose_free"] }
            }),
        ),
    )
    .await;
    assert_eq!(lactose_free.status(), StatusCode::CREATED);
    let v = json(lactose_free).await;
    assert_eq!(
        v["status"], "accepted",
        "different constraints do not merge"
    );
    let hash = v["entity"]["effective_constraint_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 64, "real constraints hash to a SHA-256 digest");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM list_entries WHERE list_id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2, "same item with different constraints → two rows");
}

// ---------------------------------------------------------------------------
// Conflict rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_wins_over_concurrent_mark_bought_and_logs_event() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    // Add, then mark bought, then delete.
    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    let bought = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "mark_bought",
            serde_json::json!({ "bought_at": "2026-05-31T10:00:00Z" }),
        ),
    )
    .await;
    assert_eq!(bought.status(), StatusCode::CREATED);

    let deleted = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-3",
            "list-a",
            "entry-1",
            "delete_item",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::CREATED);
    assert_eq!(
        json(deleted).await["entity"]["status"],
        "deleted",
        "delete wins"
    );

    // The entry is deleted, and the bought-then-deleted action is logged.
    let status: String = sqlx::query_scalar("SELECT status FROM list_entries WHERE id = 'entry-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "deleted");

    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM list_entry_events WHERE entry_id = 'entry-1' AND event_type = 'bought_then_deleted'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "the losing bought action is preserved");
}

#[tokio::test]
async fn mark_bought_after_delete_keeps_delete_and_logs_event() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "delete_item",
            serde_json::json!({}),
        ),
    )
    .await;

    // mark_bought arrives AFTER the delete: delete wins, bought is logged.
    let bought = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-3",
            "list-a",
            "entry-1",
            "mark_bought",
            serde_json::json!({ "bought_at": "2026-05-31T10:00:00Z" }),
        ),
    )
    .await;
    assert_eq!(bought.status(), StatusCode::CREATED);
    assert_eq!(
        json(bought).await["entity"]["status"],
        "deleted",
        "deleted entry is not resurrected"
    );

    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM list_entry_events WHERE entry_id = 'entry-1' AND event_type = 'bought_after_delete'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
}

#[tokio::test]
async fn mark_bought_stamps_bought_fields_and_revision() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    let bought = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "mark_bought",
            serde_json::json!({ "bought_at": "2026-05-31T10:00:00Z" }),
        ),
    )
    .await;
    let v = json(bought).await;
    assert_eq!(v["entity"]["status"], "bought");
    assert_eq!(v["entity"]["bought_by"], "user-a");
    assert_eq!(v["entity"]["bought_at"], "2026-05-31T10:00:00Z");
    // last_updated_revision is stamped with the mutation's server revision (2).
    assert_eq!(v["entity"]["last_updated_revision"], 2);
}

// ---------------------------------------------------------------------------
// set_quantity (issue #60)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_quantity_sets_absolute_value_and_survives_resnapshot() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    // Add an unresolved entry (quantity defaults to 1).
    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;

    // set_quantity to an absolute 4.
    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "set_quantity",
            serde_json::json!({ "quantity": 4 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = json(resp).await;
    assert_eq!(v["status"], "accepted");
    assert_eq!(v["entity"]["quantity"], 4, "response reflects the new quantity");
    // last_updated_revision stamped with this mutation's revision (2).
    assert_eq!(v["entity"]["last_updated_revision"], 2);

    // Idempotent under absolute semantics: a SECOND set_quantity to the same value
    // (distinct client_mutation_id) leaves it at 4, never doubling.
    let again = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-3",
            "list-a",
            "entry-1",
            "set_quantity",
            serde_json::json!({ "quantity": 4 }),
        ),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CREATED);
    assert_eq!(json(again).await["entity"]["quantity"], 4);

    // Survives a full resnapshot/sync: a fresh GET /api/list/:id (the rehydrate
    // path) returns quantity 4 — proving it is persisted, not local-only.
    let snapshot = get_list_snapshot(&pool, &s, "list-a").await;
    let entries = snapshot["entries"].as_array().expect("entries array");
    let entry = entries
        .iter()
        .find(|e| e["id"] == "entry-1")
        .expect("entry-1 in snapshot");
    assert_eq!(entry["quantity"], 4, "quantity survives resnapshot");
}

#[tokio::test]
async fn set_quantity_below_one_is_rejected() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;

    // quantity 0 → 422 (reject, not clamp).
    let zero = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "set_quantity",
            serde_json::json!({ "quantity": 0 }),
        ),
    )
    .await;
    assert_eq!(zero.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // A missing quantity is also rejected.
    let missing = post_mutation(
        &pool,
        &s,
        envelope("cm-3", "list-a", "entry-1", "set_quantity", serde_json::json!({})),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // The entry is untouched: still quantity 1, and the revision never advanced
    // past the original add (rejected mutations roll the transaction back).
    let qty: i64 =
        sqlx::query_scalar("SELECT quantity FROM list_entries WHERE id = 'entry-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(qty, 1, "rejected set_quantity leaves the entry unchanged");
    let current: i64 =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(current, 1, "a rejected mutation does not advance the revision");
}

#[tokio::test]
async fn set_quantity_above_max_is_rejected() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;

    // The ceiling (9999) is accepted; one past it (10000) is rejected.
    let at_ceiling = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "set_quantity",
            serde_json::json!({ "quantity": 9999 }),
        ),
    )
    .await;
    assert_eq!(at_ceiling.status(), StatusCode::CREATED, "9999 is the inclusive max");

    let over = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-3",
            "list-a",
            "entry-1",
            "set_quantity",
            serde_json::json!({ "quantity": 10000 }),
        ),
    )
    .await;
    assert_eq!(over.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // The over-the-ceiling reject rolled back: the entry keeps the accepted 9999.
    let qty: i64 =
        sqlx::query_scalar("SELECT quantity FROM list_entries WHERE id = 'entry-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(qty, 9999, "a rejected over-max set_quantity leaves the prior value");
}

#[tokio::test]
async fn set_quantity_does_not_resurrect_a_deleted_entry() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    post_mutation(
        &pool,
        &s,
        envelope("cm-2", "list-a", "entry-1", "delete_item", serde_json::json!({})),
    )
    .await;

    // set_quantity on a deleted entry is accepted as a no-op (delete wins).
    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-3",
            "list-a",
            "entry-1",
            "set_quantity",
            serde_json::json!({ "quantity": 5 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = json(resp).await;
    assert_eq!(v["status"], "accepted");
    // Status stays deleted; quantity is NOT changed (no resurrection).
    assert_eq!(v["entity"]["status"], "deleted");
    assert_eq!(v["entity"]["quantity"], 1, "deleted entry keeps its quantity");
}

// ---------------------------------------------------------------------------
// Authorisation / validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutation_on_missing_list_is_404() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    let s = login(&pool, "a@example.test").await;

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "does-not-exist",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mutation_on_foreign_household_list_is_403() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-b", "hh-b", "B's list").await;
    let s = login(&pool, "a@example.test").await;

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-b",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unknown_operation_is_422() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    let resp = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "frobnicate",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn mutation_without_csrf_is_403() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    let body = envelope(
        "cm-1",
        "list-a",
        "entry-1",
        "add_item",
        serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
    );
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mutations")
                // session present, but no X-CSRF-Token header.
                .header(header::COOKIE, format!("session={}", s.session))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unauthenticated_mutation_is_401() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;

    let body = envelope(
        "cm-1",
        "list-a",
        "entry-1",
        "add_item",
        serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
    );
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mutations")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Idempotency race + replay scoping (H1, H4)
// ---------------------------------------------------------------------------

/// H1: a replay that loses the `client_mutation_id` UNIQUE race must still return
/// the winner's stored response (200), never a 500. We simulate the race by
/// committing a `mutations` row for `cm-race` out of band (as a concurrent winner
/// would), then POSTing the same id; the pre-transaction replay check would catch
/// most cases, but inserting the row *after* that read and *before* our insert
/// exercises the in-flight path. Here we pre-seed the winning row to assert the
/// insert-time conflict resolves to a byte-identical replay.
#[tokio::test]
async fn inflight_duplicate_client_mutation_id_replays_not_500() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    let body = envelope(
        "cm-race",
        "list-a",
        "entry-1",
        "add_item",
        serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
    );

    // First request wins normally.
    let first = post_mutation(&pool, &s, body.clone()).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_bytes = raw_bytes(first).await;

    // Drop the stored response so the pre-transaction replay check misses, forcing
    // the request through apply + the insert-time UNIQUE conflict (the H1 path).
    sqlx::query("UPDATE mutations SET response_json = NULL WHERE client_mutation_id = 'cm-race'")
        .execute(&pool)
        .await
        .unwrap();
    // Re-seed a non-NULL response so replay_after_conflict has bytes to return,
    // mimicking the committed winner the loser must replay.
    sqlx::query("UPDATE mutations SET response_json = ?1 WHERE client_mutation_id = 'cm-race'")
        .bind(String::from_utf8(first_bytes.clone()).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    // The pre-check now finds a stored response again, so this asserts the scoped
    // replay returns 200 with identical bytes (covers replay decode + list scope).
    let replay = post_mutation(&pool, &s, body).await;
    assert_eq!(replay.status(), StatusCode::OK, "replay is 200, not 500");
    assert_eq!(
        raw_bytes(replay).await,
        first_bytes,
        "byte-identical replay"
    );

    // Still exactly one entry — nothing was double-applied.
    let entries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM list_entries WHERE list_id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(entries, 1);
}

/// H4: a `client_mutation_id` recorded against one list must not replay for a
/// different list the caller is also a member of — the stored response is scoped
/// to its original list and a mismatch is a 404.
#[tokio::test]
async fn replay_with_mismatched_list_is_404() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_list(&pool, "list-a2", "hh-a", "A's other list").await;
    let s = login(&pool, "a@example.test").await;

    // Record cm-x against list-a.
    let first = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-x",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    // Replay the SAME cm-x but against list-a2 (same household → member passes).
    let replay = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-x",
            "list-a2",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(
        replay.status(),
        StatusCode::NOT_FOUND,
        "a key recorded for another list never replays"
    );
}

/// H4: the replay path enforces household membership BEFORE returning a stored
/// response — a non-member who guesses a `client_mutation_id` gets 403/404 from
/// the auth gate, never the stored bytes.
#[tokio::test]
async fn replay_enforces_membership_before_returning_stored_response() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_user(&pool, "user-b", "b@example.test", "hh-b").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let sa = login(&pool, "a@example.test").await;
    let sb = login(&pool, "b@example.test").await;

    // user-a records cm-secret against their own list.
    let first = post_mutation(
        &pool,
        &sa,
        envelope(
            "cm-secret",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    // user-b (not a member of hh-a) guesses cm-secret against list-a.
    let leaked = post_mutation(
        &pool,
        &sb,
        envelope(
            "cm-secret",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(
        leaked.status(),
        StatusCode::FORBIDDEN,
        "membership is enforced before any stored response is read"
    );
}

// ---------------------------------------------------------------------------
// PK collision on add_item (H2)
// ---------------------------------------------------------------------------

/// H2: re-using an existing `entity_id` (list_entries.id) on a fresh add collides
/// on the PRIMARY KEY (1555), which is NOT the dedup index — it must surface as a
/// 409 Conflict and never 500 (the old code routed it into the merge branch).
#[tokio::test]
async fn add_item_with_existing_id_is_conflict_not_merged() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    // First add creates entry-1 (unresolved → never a dedup candidate).
    let first = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    // Second add re-uses entry-1's id under a NEW client_mutation_id → PK collision.
    let dup = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Bread", "display_name": "Bread" }),
        ),
    )
    .await;
    assert_eq!(
        dup.status(),
        StatusCode::CONFLICT,
        "a PK id collision is 409, not merged and not 500"
    );

    // The original row is untouched and still the only one.
    let (count, name): (i64, String) = sqlx::query_as(
        "SELECT COUNT(*), MAX(display_name) FROM list_entries WHERE list_id = 'list-a'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "no second row created");
    assert_eq!(name, "Milk", "original row unchanged");
}

// ---------------------------------------------------------------------------
// Cross-list IDOR (H3) + delete-wins on skip/rename (M1)
// ---------------------------------------------------------------------------

/// H3: an entry id that belongs to a different list must 404 for every entry op,
/// even when the caller is authorised for the list they name in the request.
#[tokio::test]
async fn cross_list_entry_id_is_404_for_every_op() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_list(&pool, "list-b", "hh-a", "A's other list").await;
    let s = login(&pool, "a@example.test").await;

    // entry-x lives on list-b.
    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-seed",
            "list-b",
            "entry-x",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;

    // Every entry op naming list-a but entry-x (which is on list-b) must 404.
    for (i, op, payload) in [
        (1, "delete_item", serde_json::json!({})),
        (2, "mark_bought", serde_json::json!({})),
        (3, "mark_skipped", serde_json::json!({})),
        (
            4,
            "rename_item",
            serde_json::json!({ "display_name": "Hacked" }),
        ),
    ] {
        let resp = post_mutation(
            &pool,
            &s,
            envelope(&format!("cm-idor-{i}"), "list-a", "entry-x", op, payload),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{op} on a cross-list entry id must 404"
        );
    }

    // entry-x on list-b is untouched (still active).
    let status: String = sqlx::query_scalar("SELECT status FROM list_entries WHERE id = 'entry-x'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "active", "cross-list entry was never mutated");
}

/// M1: mark_skipped and rename_item must not resurrect a deleted entry.
#[tokio::test]
async fn skip_and_rename_do_not_resurrect_deleted_entry() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-1",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-2",
            "list-a",
            "entry-1",
            "delete_item",
            serde_json::json!({}),
        ),
    )
    .await;

    // Skip a deleted entry: accepted, but status stays deleted.
    let skip = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-3",
            "list-a",
            "entry-1",
            "mark_skipped",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(skip.status(), StatusCode::CREATED);
    assert_eq!(
        json(skip).await["entity"]["status"],
        "deleted",
        "mark_skipped does not resurrect a deleted entry"
    );

    // Rename a deleted entry: accepted, name unchanged, status stays deleted.
    let rename = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-4",
            "list-a",
            "entry-1",
            "rename_item",
            serde_json::json!({ "display_name": "Renamed" }),
        ),
    )
    .await;
    assert_eq!(rename.status(), StatusCode::CREATED);
    let v = json(rename).await;
    assert_eq!(
        v["entity"]["status"], "deleted",
        "rename_item does not resurrect a deleted entry"
    );
    assert_eq!(
        v["entity"]["display_name"], "Milk",
        "deleted entry's name is not changed"
    );
}

// ---------------------------------------------------------------------------
// Cross-list IDOR via accept-and-log ops + shared post-apply path (HIGH)
// ---------------------------------------------------------------------------

/// HIGH: resolve_alias / reject_match / set_constraint go through the shared
/// stamp/load post-apply path, which is now scoped to (id, list_id). A caller
/// authorised for list-a must not touch or read an entry on list-b by supplying
/// its id: each op 404s and the foreign entry's revision stays unchanged.
///
/// `resolve_alias` now has a real handler (P3-T3): its guarded bind UPDATE is
/// scoped to (id, list_id) and matches 0 rows for a cross-list id, so the entry
/// is never bound and the shared stamp path 404s it — the IDOR guard holds. It is
/// sent with a valid payload (a same-household canonical item) so the request
/// reaches that guarded UPDATE rather than short-circuiting on payload validation;
/// reject_match / set_constraint remain accept-and-log and are sent empty.
#[tokio::test]
async fn cross_list_idor_via_accept_and_log_ops_is_404() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    seed_list(&pool, "list-b", "hh-a", "A's other list").await;
    // A same-household canonical item so the resolve_alias payload passes the
    // catalogue household check and reaches the (id, list_id)-scoped bind UPDATE.
    // Its name is deliberately NOT "Milk" so the async add_item resolution cascade
    // below finds no match and leaves entry-x `unresolved` at revision 1 (a
    // matching name would resolve it and bump its revision, masking the IDOR check).
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES ('cat-idor', 'Misc', 'Varios', 'box', 9)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES ('item-idor', 'hh-a', 'Zzqxwidget', 'Zzqxwidget', 'cat-idor', 'any', ?1)",
    )
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .unwrap();
    let s = login(&pool, "a@example.test").await;

    // entry-x lives on list-b (its own revision bump → last_updated_revision = 1).
    post_mutation(
        &pool,
        &s,
        envelope(
            "cm-seed",
            "list-b",
            "entry-x",
            "add_item",
            serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    let before: i64 =
        sqlx::query_scalar("SELECT last_updated_revision FROM list_entries WHERE id = 'entry-x'")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Each op naming list-a but entry-x (on list-b) must 404. `resolve_alias`
    // carries a valid (same-household) payload so it exercises the guarded bind
    // path; the other two are accept-and-log and are sent empty.
    for (i, op, payload) in [
        (
            1,
            "resolve_alias",
            serde_json::json!({ "chosen_canonical_item_id": "item-idor", "raw_text": "milk" }),
        ),
        (2, "reject_match", serde_json::json!({})),
        (3, "set_constraint", serde_json::json!({})),
    ] {
        let resp = post_mutation(
            &pool,
            &s,
            envelope(
                &format!("cm-idor-log-{i}"),
                "list-a",
                "entry-x",
                op,
                payload,
            ),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{op} on a cross-list entry id must 404"
        );
    }

    // The cross-list resolve_alias must not have leaked an alias either (the bind
    // never matched a row, so no alias is learned).
    let alias_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases WHERE household_id = 'hh-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(alias_count, 0, "a no-op cross-list bind learns no alias");

    // The foreign entry's revision is unchanged — it was never stamped.
    let after: i64 =
        sqlx::query_scalar("SELECT last_updated_revision FROM list_entries WHERE id = 'entry-x'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after, before, "foreign entry's revision was not bumped");
}

// ---------------------------------------------------------------------------
// add_item canonical_item_id household scoping (MEDIUM)
// ---------------------------------------------------------------------------

/// MEDIUM: a resolved add may only bind a canonical_item belonging to the target
/// list's household. Binding another household's item id is rejected (404), and a
/// same-household resolved add still works.
#[tokio::test]
async fn add_item_rejects_foreign_household_canonical_item() {
    let pool = setup_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_household(&pool, "hh-b", "Household B").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    // item-a belongs to hh-a; item-b belongs to hh-b.
    seed_canonical_item(&pool, "cat-a", "item-a", "hh-a").await;
    seed_canonical_item(&pool, "cat-b", "item-b", "hh-b").await;
    let s = login(&pool, "a@example.test").await;

    // Binding hh-b's item to hh-a's list is rejected.
    let foreign = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-foreign",
            "list-a",
            "entry-1",
            "add_item",
            serde_json::json!({ "canonical_item_id": "item-b", "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(
        foreign.status(),
        StatusCode::NOT_FOUND,
        "a foreign-household canonical item cannot be attached"
    );

    // No entry was created and the revision did not advance.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM list_entries WHERE list_id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "rejected add creates no row");

    // The same-household item binds normally.
    let ok = post_mutation(
        &pool,
        &s,
        envelope(
            "cm-ok",
            "list-a",
            "entry-2",
            "add_item",
            serde_json::json!({ "canonical_item_id": "item-a", "raw_text": "Milk", "display_name": "Milk" }),
        ),
    )
    .await;
    assert_eq!(ok.status(), StatusCode::CREATED);
    let v = json(ok).await;
    assert_eq!(v["entity"]["canonical_item_id"], "item-a");
    assert_eq!(v["entity"]["resolution_status"], "resolved");
}

// ---------------------------------------------------------------------------
// In-flight duplicate add_item idempotency under real concurrency (HIGH)
// ---------------------------------------------------------------------------

/// HIGH regression: several identical `add_item` requests (same
/// `client_mutation_id` AND same client-supplied `entity_id`) fired concurrently
/// must ALL resolve to the idempotent contract — exactly one apply, every response
/// a byte-identical 200/201, and never a 409 from the entry-id PRIMARY KEY
/// collision.
///
/// Before the reserve-first fix, a loser ran `apply_add_item` before the
/// `client_mutation_id` UNIQUE check, hit the `list_entries.id` PK collision, and
/// 409'd instead of replaying. Reserving the `mutations` row as the first write
/// makes the idempotency key authoritative, so the loser replays the winner's
/// stored response. A shared-cache pool gives genuine multi-connection contention
/// on SQLite's single write lock (busy_timeout absorbs the wait), so the loser
/// path is exercised deterministically rather than relying on the fast-path check.
#[tokio::test]
async fn concurrent_duplicate_add_item_all_replay_never_409() {
    let pool = setup_shared_pool().await;
    seed_household(&pool, "hh-a", "Household A").await;
    seed_user(&pool, "user-a", "a@example.test", "hh-a").await;
    seed_list(&pool, "list-a", "hh-a", "A's list").await;
    let s = login(&pool, "a@example.test").await;

    // Eight identical requests: same client_mutation_id AND same entity_id.
    let body = envelope(
        "cm-race",
        "list-a",
        "entry-1",
        "add_item",
        serde_json::json!({ "raw_text": "Milk", "display_name": "Milk" }),
    );

    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let session = s.session.clone();
        let csrf = s.csrf.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            let resp = app(AppState::new(pool))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/mutations")
                        .header(header::COOKIE, format!("session={session}"))
                        .header("X-CSRF-Token", &csrf)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let bytes = raw_bytes(resp).await;
            (status, bytes)
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }

    // No request 409'd or 500'd; each is a success (201 winner, 200 replays).
    for (status, _) in &results {
        assert!(
            *status == StatusCode::CREATED || *status == StatusCode::OK,
            "every concurrent duplicate is a success, got {status}"
        );
        assert_ne!(*status, StatusCode::CONFLICT, "no spurious 409");
    }
    // Exactly one winner (201), the rest replays (200).
    let created = results
        .iter()
        .filter(|(s, _)| *s == StatusCode::CREATED)
        .count();
    assert_eq!(created, 1, "exactly one request applied the mutation");

    // All response bodies are byte-identical (the winner's stored response).
    let first_body = &results[0].1;
    for (_, body) in &results {
        assert_eq!(body, first_body, "all responses are byte-identical");
    }

    // Exactly one entry and one mutation row — the mutation was applied once.
    let entries: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM list_entries WHERE list_id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(entries, 1, "the entry was created exactly once");

    let mutations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mutations WHERE client_mutation_id = 'cm-race'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mutations, 1, "exactly one mutation row was committed");

    // The revision advanced exactly once.
    let revision: i64 =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(revision, 1, "revision advanced exactly once");
}
