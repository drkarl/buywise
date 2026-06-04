//! Integration tests for the P3-T1 server-side resolution cascade (issue #25).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the FULL migrations applied — including `002_seed.sql`,
//! which seeds the bilingual catalogue (canonical items + aliases) for the
//! household `00000000-0000-0000-0000-000000000001`. Tests put their user / list
//! in that seeded household so the cascade resolves against the real catalogue.
//!
//! Coverage (the issue's acceptance criteria + testing expectations):
//!   * `add_item "leche"` → entry transitions to `resolved` with the Milk
//!     canonical id within 500ms (the mutation is accepted immediately).
//!   * `add_item "cream"` → entry transitions to `ambiguous` with candidate ids.
//!   * `add_item "xyzqrst_unknown"` → entry stays `unresolved`.
//!   * two offline "leche" adds → both resolve to Milk; the loser is `deleted`
//!     with a `merged` audit event, the survivor's quantity is 2 (dedup-on-
//!     resolution merge).
//!   * a DB error injected into the spawned task (the canonical item it would
//!     bind is deleted out from under it) leaves the entry `unresolved` and never
//!     corrupts the committed mutation.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use sqlx::SqlitePool;
use std::time::{Duration, Instant};
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
/// The household the seed (`002_seed.sql`) populates the catalogue for.
const SEED_HOUSEHOLD: &str = "00000000-0000-0000-0000-000000000001";
/// Canonical id of "Milk" in the seed (deterministic SHA-256 slug) — the target
/// of the "leche" / "milk" aliases.
const MILK_ID: &str = "2c41ac7cde2aad18bbf43e93541efe13da56585f0e8965ed93a5b6e60d6b01e8";

struct Session {
    session: String,
    csrf: String,
}

/// A fresh in-memory pool with migrations AND the seed catalogue applied.
async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed a member user and an empty list in the catalogue household (the household
/// row itself is already created by `004_seed_phase1.sql`), then log the user in.
async fn seed_seed_household_user_and_list(pool: &SqlitePool) -> Session {
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES ('user-a', 'a@example.test', ?1, 'user-a', ?2)",
    )
    .bind(&hash)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");

    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, 'user-a', 'owner', ?2)",
    )
    .bind(SEED_HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");

    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES ('list-a', ?1, 'A list', 'active', 0, ?2)",
    )
    .bind(SEED_HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");

    login(pool, "a@example.test").await
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
    state: &AppState,
    s: &Session,
    body: serde_json::Value,
) -> axum::response::Response {
    app(state.clone())
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

fn add_item(client_mutation_id: &str, entity_id: &str, raw_text: &str) -> serde_json::Value {
    serde_json::json!({
        "client_mutation_id": client_mutation_id,
        "device_id": "device-1",
        "list_id": "list-a",
        "base_revision": 0,
        "entity_type": "list_entry",
        "entity_id": entity_id,
        "operation_type": "add_item",
        "payload": { "raw_text": raw_text, "display_name": raw_text },
        "created_at_client": "2026-06-04T00:00:00.000Z",
    })
}

/// Poll the entry's `resolution_status` until `predicate` holds or 500ms elapses.
/// Returns the matching row, or panics with the last seen state on timeout.
async fn poll_entry(
    pool: &SqlitePool,
    entry_id: &str,
    predicate: impl Fn(&EntryState) -> bool,
) -> EntryState {
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut last = None;
    loop {
        if let Some(state) = entry_state(pool, entry_id).await {
            if predicate(&state) {
                return state;
            }
            last = Some(state);
        }
        if Instant::now() >= deadline {
            panic!("entry {entry_id} did not reach expected state within 500ms; last = {last:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Debug, Clone)]
struct EntryState {
    resolution_status: String,
    canonical_item_id: Option<String>,
    resolution_candidates_json: Option<String>,
    status: String,
    quantity: i64,
}

/// (resolution_status, canonical_item_id, resolution_candidates_json, status, quantity)
type EntryStateRow = (String, Option<String>, Option<String>, String, i64);

async fn entry_state(pool: &SqlitePool, entry_id: &str) -> Option<EntryState> {
    let row: Option<EntryStateRow> = sqlx::query_as(
        "SELECT resolution_status, canonical_item_id, resolution_candidates_json, status, quantity \
         FROM list_entries WHERE id = ?1",
    )
    .bind(entry_id)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(
        |(resolution_status, canonical_item_id, resolution_candidates_json, status, quantity)| {
            EntryState {
                resolution_status,
                canonical_item_id,
                resolution_candidates_json,
                status,
                quantity,
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Acceptance criteria
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adding_leche_resolves_to_milk() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = json(resp).await;
    // The mutation is accepted immediately, BEFORE resolution runs.
    assert_eq!(v["entity"]["resolution_status"], "unresolved");

    // After a short delay the spawned cascade resolves it to Milk.
    let resolved = poll_entry(&pool, "entry-1", |e| e.resolution_status == "resolved").await;
    assert_eq!(resolved.canonical_item_id.as_deref(), Some(MILK_ID));
    assert_eq!(resolved.status, "active");
}

#[tokio::test]
async fn adding_cream_becomes_ambiguous_with_candidates() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "cream")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let ambiguous = poll_entry(&pool, "entry-1", |e| e.resolution_status == "ambiguous").await;
    assert!(
        ambiguous.canonical_item_id.is_none(),
        "ambiguous keeps NULL canonical"
    );
    let candidates: Vec<String> =
        serde_json::from_str(ambiguous.resolution_candidates_json.as_deref().unwrap()).unwrap();
    assert!(
        candidates.len() >= 2,
        "cream must surface multiple candidates, got {candidates:?}"
    );
}

#[tokio::test]
async fn adding_unknown_term_stays_unresolved() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "xyzqrst_unknown")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Give the spawned task time to run; it must leave the entry unresolved.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let state = entry_state(&pool, "entry-1").await.unwrap();
    assert_eq!(state.resolution_status, "unresolved");
    assert!(state.canonical_item_id.is_none());
    assert!(state.resolution_candidates_json.is_none());
}

// ---------------------------------------------------------------------------
// Dedup-on-resolution merge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_offline_leche_adds_merge_on_resolution() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    // Two independent unresolved adds (different entry ids → no merge at add time).
    let first = post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche")).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let second = post_mutation(&state, &s, add_item("cm-2", "entry-2", "leche")).await;
    assert_eq!(second.status(), StatusCode::CREATED);

    // Wait until exactly one active resolved Milk row with quantity 2 survives.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let (active, qty): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(MAX(quantity), 0) FROM list_entries \
             WHERE list_id = 'list-a' AND status = 'active' AND canonical_item_id = ?1",
        )
        .bind(MILK_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
        if active == 1 && qty == 2 {
            break;
        }
        if Instant::now() >= deadline {
            panic!("entries did not merge within 500ms (active={active}, qty={qty})");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Exactly one of the two entries is deleted (the merge loser).
    let deleted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM list_entries \
         WHERE id IN ('entry-1', 'entry-2') AND status = 'deleted'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deleted, 1, "one entry is deleted as the merge loser");

    // A 'merged' audit event points the loser at the survivor.
    let (loser, payload): (String, String) = sqlx::query_as(
        "SELECT entry_id, payload FROM list_entry_events WHERE event_type = 'merged'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let survivor = payload["merged_into"].as_str().unwrap();
    assert!(["entry-1", "entry-2"].contains(&loser.as_str()));
    assert!(["entry-1", "entry-2"].contains(&survivor));
    assert_ne!(loser, survivor, "loser merges into the OTHER entry");

    // The surviving entry is the active one with quantity 2.
    let survivor_state = entry_state(&pool, survivor).await.unwrap();
    assert_eq!(survivor_state.status, "active");
    assert_eq!(survivor_state.quantity, 2);
}

// ---------------------------------------------------------------------------
// Failure isolation: a DB error in the spawned task must not corrupt state
// ---------------------------------------------------------------------------

/// The cascade runs AFTER the mutation has committed, so a DB error inside the
/// spawned task must never affect the committed mutation state.
///
/// We force a deterministic DB error in the resolution write: a bogus alias
/// "zzzuniqueterm" is pointed at a NON-EXISTENT canonical id (inserted with FK
/// enforcement toggled off for that one statement). Step 1 of the cascade returns
/// a `SingleMatch` on that id, but the resolution UPDATE that binds it then
/// violates the `list_entries.canonical_item_id` foreign key — a sqlx error in
/// the spawned task. The committed mutation (and its entry) must be untouched.
#[tokio::test]
async fn resolution_db_failure_does_not_corrupt_committed_mutation() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    // Insert an alias pointing at a canonical id that does not exist. The alias
    // table FK-references canonical_items, so disable FK enforcement for just this
    // INSERT (the in-memory pool is single-connection, so the pragma is scoped to
    // this test's connection and restored immediately after).
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO item_aliases (id, household_id, canonical_item_id, alias, language, source, created_at) \
         VALUES ('bogus-alias', ?1, 'does-not-exist-canonical-id', 'zzzuniqueterm', 'en', 'manual', ?2)",
    )
    .bind(SEED_HOUSEHOLD)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .unwrap();

    // The add commits immediately (accepted), then the spawned cascade resolves
    // "zzzuniqueterm" → SingleMatch('does-not-exist-canonical-id') and the bind
    // UPDATE fails on the FK.
    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "zzzuniqueterm")).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "mutation accepted immediately, before resolution runs"
    );

    // Let the spawned task run and fail.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The committed mutation row is intact: accepted, revision 1.
    let (status, revision): (String, Option<i64>) = sqlx::query_as(
        "SELECT status, server_revision FROM mutations WHERE client_mutation_id = 'cm-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "accepted", "committed mutation status unchanged");
    assert_eq!(revision, Some(1), "committed revision unchanged");

    // The entry is intact: still active and still unresolved (the failed bind was
    // rolled back, never half-applied). The list revision did not advance past 1.
    let entry = entry_state(&pool, "entry-1").await.unwrap();
    assert_eq!(entry.status, "active");
    assert_eq!(
        entry.resolution_status, "unresolved",
        "a failed resolution leaves the entry unresolved, never partially bound"
    );
    assert!(entry.canonical_item_id.is_none());

    let current_revision: i64 =
        sqlx::query_scalar("SELECT current_revision FROM shopping_lists WHERE id = 'list-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        current_revision, 1,
        "the failed resolution did not bump the list revision"
    );
}
