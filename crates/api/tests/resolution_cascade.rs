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
use buywise_api::llm::{LlmClient, LlmError};
use buywise_api::sse::SseEvent;
use buywise_api::{app, sse, AppState};
use buywise_core::constraint_hash::effective_constraint_hash;
use buywise_core::db;
use http_body_util::BodyExt;
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
    add_item_with_constraints(client_mutation_id, entity_id, raw_text, None)
}

/// Like [`add_item`] but with an optional `constraints` payload. A non-empty
/// constraints set survives onto the entry and is fed into the recomputed
/// `effective_constraint_hash` when the cascade resolves the entry.
fn add_item_with_constraints(
    client_mutation_id: &str,
    entity_id: &str,
    raw_text: &str,
    constraints: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "raw_text": raw_text, "display_name": raw_text });
    if let Some(constraints) = constraints {
        payload["constraints"] = constraints;
    }
    serde_json::json!({
        "client_mutation_id": client_mutation_id,
        "device_id": "device-1",
        "list_id": "list-a",
        "base_revision": 0,
        "entity_type": "list_entry",
        "entity_id": entity_id,
        "operation_type": "add_item",
        "payload": payload,
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

    // The unknown term we expect to stay unresolved.
    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "xyzqrst_unknown")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A "leche" sentinel posted AFTER the unknown. Polling it to `resolved` proves
    // the spawned cascade has actually run for adds enqueued up to this point, so a
    // still-`unresolved` "entry-1" below is a real NoMatch — not a task that simply
    // had not been scheduled yet (which a fixed sleep could not distinguish).
    let sentinel = post_mutation(&state, &s, add_item("cm-2", "sentinel", "leche")).await;
    assert_eq!(sentinel.status(), StatusCode::CREATED);
    poll_entry(&pool, "sentinel", |e| e.resolution_status == "resolved").await;

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

    // A "leche" sentinel posted AFTER the failing add. Polling it to `resolved`
    // proves the cascade has run (and failed) for the earlier add, rather than
    // relying on a fixed sleep that could pass simply because the failing task had
    // not been scheduled yet.
    let sentinel = post_mutation(&state, &s, add_item("cm-2", "sentinel", "leche")).await;
    assert_eq!(sentinel.status(), StatusCode::CREATED);
    poll_entry(&pool, "sentinel", |e| e.resolution_status == "resolved").await;

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

    // The failed resolution never bumped a revision: entry-1 still carries the
    // revision its (cm-1) mutation commit stamped (1), unchanged by the cascade.
    // (The successful sentinel resolution advances the list's current_revision, so
    // we assert on this entry's own stamp rather than the global counter.)
    let entry_revision: i64 =
        sqlx::query_scalar("SELECT last_updated_revision FROM list_entries WHERE id = 'entry-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        entry_revision, 1,
        "the failed resolution did not bump entry-1's revision"
    );

    // The failed resolution wrote NO synthetic replay row for entry-1: only the
    // client's original add (device_id != 'server') touched it. A server-origin
    // row would mean a revision bump leaked without its entry change (the rollback
    // must take both or neither — atomicity of the revision-bump + replay-row).
    let server_rows_for_entry1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mutations WHERE entity_id = 'entry-1' AND device_id = 'server'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        server_rows_for_entry1, 0,
        "a failed resolution writes no replayable mutations row"
    );
}

// ---------------------------------------------------------------------------
// Reconnect/backfill: every resolution outcome writes a replayable `mutations`
// row, so a client that disconnected before the async resolution still receives
// the outcome on reconnect via `?since=` (sse::backfill_since).
// ---------------------------------------------------------------------------

/// The single `mutation` event `backfill_since(list, since)` replays at exactly
/// `revision`, or a panic if the count is not one. Returns its entity.
async fn backfill_one(pool: &SqlitePool, since: i64, revision: i64) -> serde_json::Value {
    let events = sse::backfill_since(pool, "list-a", since).await.unwrap();
    let at_revision: Vec<&SseEvent> = events
        .iter()
        .filter(|e| match e {
            SseEvent::Mutation {
                server_revision, ..
            } => *server_revision == revision,
        })
        .collect();
    assert_eq!(
        at_revision.len(),
        1,
        "exactly one backfilled event at revision {revision}, got {}",
        at_revision.len()
    );
    match at_revision[0] {
        SseEvent::Mutation { entity, .. } => entity.clone(),
    }
}

/// The entry's `last_updated_revision` (the revision a backfill replays it at).
async fn entry_revision(pool: &SqlitePool, entry_id: &str) -> i64 {
    sqlx::query_scalar("SELECT last_updated_revision FROM list_entries WHERE id = ?1")
        .bind(entry_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The id of the sole active Milk entry on list-a (the merge survivor).
async fn active_milk_entry(pool: &SqlitePool) -> String {
    sqlx::query_scalar(
        "SELECT id FROM list_entries \
         WHERE list_id = 'list-a' AND status = 'active' AND canonical_item_id = ?1",
    )
    .bind(MILK_ID)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn resolved_outcome_is_replayable_on_reconnect() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resolved = poll_entry(&pool, "entry-1", |e| e.resolution_status == "resolved").await;
    assert_eq!(resolved.canonical_item_id.as_deref(), Some(MILK_ID));

    // The resolution landed at the entry's stamped revision (2: add=1, resolve=2).
    let revision = entry_revision(&pool, "entry-1").await;
    // A client that saw the unresolved add at revision 1 and reconnects with
    // ?since=1 must backfill the resolved entity at the resolution revision.
    let entity = backfill_one(&pool, revision - 1, revision).await;
    assert_eq!(entity["id"], "entry-1");
    assert_eq!(entity["resolution_status"], "resolved");
    assert_eq!(entity["canonical_item_id"], MILK_ID);
}

#[tokio::test]
async fn ambiguous_outcome_is_replayable_on_reconnect() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "cream")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    poll_entry(&pool, "entry-1", |e| e.resolution_status == "ambiguous").await;

    let revision = entry_revision(&pool, "entry-1").await;
    let entity = backfill_one(&pool, revision - 1, revision).await;
    assert_eq!(entity["id"], "entry-1");
    assert_eq!(entity["resolution_status"], "ambiguous");
    assert!(
        entity["canonical_item_id"].is_null(),
        "ambiguous keeps NULL canonical in the replayed entity"
    );
    assert!(
        entity["resolution_candidates_json"].is_string(),
        "replayed ambiguous entity carries the candidate ids"
    );
}

#[tokio::test]
async fn merge_outcome_replays_both_survivor_and_loser_on_reconnect() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    assert_eq!(
        post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche"))
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        post_mutation(&state, &s, add_item("cm-2", "entry-2", "leche"))
            .await
            .status(),
        StatusCode::CREATED
    );

    // Wait for the merge: one active resolved Milk row with quantity 2.
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

    // Identify survivor (active) and loser (deleted) — which add wins is not
    // deterministic, so derive each from the committed state.
    let survivor_id = active_milk_entry(&pool).await;
    let loser_id = if survivor_id == "entry-1" {
        "entry-2"
    } else {
        "entry-1"
    };

    // The merge now bumps the revision TWICE: survivor at R, loser at R+1.
    let rev_survivor = entry_revision(&pool, &survivor_id).await;
    let rev_loser = entry_revision(&pool, loser_id).await;
    assert_eq!(
        rev_survivor + 1,
        rev_loser,
        "survivor and loser are at consecutive revisions (R, R+1)"
    );

    // Backfill from just before the survivor's revision replays BOTH events, in
    // ascending revision order: survivor (quantity 2) then loser (deleted).
    let events = sse::backfill_since(&pool, "list-a", rev_survivor - 1)
        .await
        .unwrap();
    let merge_events: Vec<(i64, &serde_json::Value)> = events
        .iter()
        .filter_map(|e| {
            let SseEvent::Mutation {
                server_revision,
                entity,
            } = e;
            (*server_revision >= rev_survivor).then_some((*server_revision, entity))
        })
        .collect();
    assert_eq!(
        merge_events.len(),
        2,
        "the merge replays two events (survivor at R, loser at R+1)"
    );
    assert_eq!(
        merge_events[0].0, rev_survivor,
        "survivor event replays first (R)"
    );
    assert_eq!(
        merge_events[1].0, rev_loser,
        "loser event replays second (R+1)"
    );

    let survivor = merge_events[0].1;
    assert_eq!(survivor["status"], "active");
    assert_eq!(survivor["resolution_status"], "resolved");
    assert_eq!(survivor["quantity"], 2, "survivor replays with quantity 2");

    let loser = merge_events[1].1;
    assert_eq!(loser["status"], "deleted");
    assert_eq!(
        loser["id"].as_str().unwrap(),
        loser_id,
        "loser deletion replays at R+1"
    );
}

/// Regression for the same-revision merge bug (review High): a client that
/// processed only the survivor event (advancing its cursor to R) and reconnected
/// with `?since=R` MUST still receive the loser's deletion at R+1. With both
/// events at a shared revision R, `R > R` is false and the deletion was lost
/// forever; distinct revisions make it recoverable.
#[tokio::test]
async fn merge_loser_deletion_replays_after_survivor_only_reconnect() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    assert_eq!(
        post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche"))
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        post_mutation(&state, &s, add_item("cm-2", "entry-2", "leche"))
            .await
            .status(),
        StatusCode::CREATED
    );

    // Wait for the merge: one active resolved Milk row with quantity 2.
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

    let survivor_id = active_milk_entry(&pool).await;
    let loser_id = if survivor_id == "entry-1" {
        "entry-2"
    } else {
        "entry-1"
    };
    let rev_survivor = entry_revision(&pool, &survivor_id).await;
    let rev_loser = entry_revision(&pool, loser_id).await;

    // Simulate a client that saw the survivor event (cursor → R) then reconnected.
    // `?since=R` must still replay the loser's deletion at R+1 — the bug was that
    // it did not, because the loser also lived at R.
    let after_survivor = sse::backfill_since(&pool, "list-a", rev_survivor)
        .await
        .unwrap();
    let loser_events: Vec<&serde_json::Value> = after_survivor
        .iter()
        .filter_map(|e| {
            let SseEvent::Mutation {
                server_revision,
                entity,
            } = e;
            (*server_revision == rev_loser).then_some(entity)
        })
        .collect();
    assert_eq!(
        loser_events.len(),
        1,
        "the loser deletion IS replayed after a survivor-only (?since=R) reconnect"
    );
    assert_eq!(loser_events[0]["status"], "deleted");
    assert_eq!(loser_events[0]["id"].as_str().unwrap(), loser_id);

    // And a fuller reconnect (?since=R-1) still returns BOTH events.
    let both = sse::backfill_since(&pool, "list-a", rev_survivor - 1)
        .await
        .unwrap();
    let count_in_merge = both
        .iter()
        .filter(|e| {
            let SseEvent::Mutation {
                server_revision, ..
            } = e;
            *server_revision >= rev_survivor
        })
        .count();
    assert_eq!(
        count_in_merge, 2,
        "?since=R-1 still replays both merge events"
    );
}

// ---------------------------------------------------------------------------
// effective_constraint_hash recompute: a resolution with real constraints must
// recompute the hash from them, NOT fall back to the `default` sentinel.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolving_an_entry_with_constraints_recomputes_a_non_default_hash() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    // "leche" with a real constraint set. The add commits unresolved (constraints
    // alone do not resolve it), then the cascade resolves it to Milk and must
    // recompute the hash from these constraints rather than hardcoding `default`.
    let constraints = serde_json::json!({ "brand": "Hacendado", "fat": "semi" });
    let resp = post_mutation(
        &state,
        &s,
        add_item_with_constraints("cm-1", "entry-1", "leche", Some(constraints.clone())),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resolved = poll_entry(&pool, "entry-1", |e| e.resolution_status == "resolved").await;
    assert_eq!(resolved.canonical_item_id.as_deref(), Some(MILK_ID));

    // The stored hash must equal the hash of the constraints, and must not be the
    // `default` sentinel — the issue's "never hardcode default unless constraints
    // empty" requirement.
    let hash: String = sqlx::query_scalar(
        "SELECT effective_constraint_hash FROM list_entries WHERE id = 'entry-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let expected = effective_constraint_hash("entry-1", Some(MILK_ID), Some(&constraints));
    assert_ne!(
        hash, "default",
        "constraints present must not hash to default"
    );
    assert_eq!(
        hash, expected,
        "stored hash must equal effective_constraint_hash(constraints)"
    );
}

// ---------------------------------------------------------------------------
// SSE broadcast: the cascade emits a `mutation` event for each resolution
// outcome (and two for a dedup merge) — the issue's explicit acceptance criterion.
// ---------------------------------------------------------------------------

/// Subscribe to a list's broadcast channel and receive the next `mutation` event,
/// or panic if none arrives within 500ms.
async fn recv_event(
    rx: &mut tokio::sync::broadcast::Receiver<SseEvent>,
) -> (i64, serde_json::Value) {
    let event = tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .expect("an SSE event within 500ms")
        .expect("broadcast channel open");
    match event {
        SseEvent::Mutation {
            server_revision,
            entity,
        } => (server_revision, entity),
    }
}

#[tokio::test]
async fn resolution_broadcasts_an_sse_event() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    // Subscribe BEFORE posting so the channel exists when the cascade broadcasts.
    let mut rx = sse::sender_for(&state, "list-a").subscribe();

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche")).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The mutation commit itself broadcasts the unresolved add (revision 1); the
    // cascade then broadcasts the resolved entry. Drain until we see the resolution
    // event (resolution_status == "resolved").
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let (revision, entity) = recv_event(&mut rx).await;
        if entity["resolution_status"] == "resolved" {
            assert_eq!(entity["id"], "entry-1");
            assert_eq!(entity["canonical_item_id"], MILK_ID);
            assert!(revision >= 1, "resolution event carries a list revision");
            break;
        }
        if Instant::now() >= deadline {
            panic!("no resolved SSE event broadcast for the resolution outcome");
        }
    }
}

#[tokio::test]
async fn dedup_merge_broadcasts_two_sse_events() {
    let pool = setup_pool().await;
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let mut rx = sse::sender_for(&state, "list-a").subscribe();

    // Two unresolved "leche" adds. The second to resolve hits the dedup index and
    // merges into the survivor, which must broadcast TWO events: the survivor's
    // updated quantity and the loser's deletion.
    assert_eq!(
        post_mutation(&state, &s, add_item("cm-1", "entry-1", "leche"))
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        post_mutation(&state, &s, add_item("cm-2", "entry-2", "leche"))
            .await
            .status(),
        StatusCode::CREATED
    );

    // Collect events until we have observed both a quantity-2 survivor and a
    // deleted loser among the broadcast entities (the two merge outcomes).
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut saw_survivor = false;
    let mut saw_deleted = false;
    while !(saw_survivor && saw_deleted) {
        if Instant::now() >= deadline {
            panic!(
                "merge did not broadcast both events (survivor={saw_survivor}, deleted={saw_deleted})"
            );
        }
        let (_, entity) = recv_event(&mut rx).await;
        if entity["status"] == "deleted" {
            saw_deleted = true;
        } else if entity["status"] == "active"
            && entity["resolution_status"] == "resolved"
            && entity["quantity"] == 2
        {
            saw_survivor = true;
        }
    }
}

// ---------------------------------------------------------------------------
// P3-T2: LLM fallback for NoMatch (issue #26)
//
// These drive the cascade with a FAKE `LlmClient` injected via
// `AppState::with_llm_client`, so step 3 runs deterministically without a network
// call. The fake records how many times it was invoked, so a test can prove the
// LLM was (or was not) called.
// ---------------------------------------------------------------------------

/// A genuinely-unknown-but-plausible raw term: it has NO seeded alias and does
/// not fuzzy-match any catalogue name, so the cascade reaches step 3 (the LLM).
/// (The issue's "zumo de manzana" example is already a seeded alias, so it would
/// resolve at step 1 and never exercise the fallback — we use a novel term.)
const UNKNOWN_TERM: &str = "qwxzv nonsenseitem";
/// The canonical `name_en` the fake LLM answers with for [`UNKNOWN_TERM`].
const LLM_TARGET_NAME: &str = "Apple juice";

/// A fake LLM client that always answers with `answer` (None → "no match") and
/// counts its invocations.
struct FakeLlm {
    answer: Option<String>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmClient for FakeLlm {
    async fn resolve_item(
        &self,
        _raw_text: &str,
        _candidate_names: &[String],
    ) -> Result<Option<String>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.clone())
    }
}

/// A fake LLM client that always fails (simulates a timeout / rate limit) and
/// counts its invocations.
struct FailingLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmClient for FailingLlm {
    async fn resolve_item(
        &self,
        _raw_text: &str,
        _candidate_names: &[String],
    ) -> Result<Option<String>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(LlmError::Status(429))
    }
}

/// Look up a seeded canonical id by its exact `name_en`.
async fn id_by_name_en(pool: &SqlitePool, name_en: &str) -> String {
    sqlx::query_scalar("SELECT id FROM canonical_items WHERE name_en = ?1 AND household_id = ?2")
        .bind(name_en)
        .bind(SEED_HOUSEHOLD)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|_| panic!("seed has a canonical item named {name_en:?}"))
}

/// Count `source='llm'` rows in `api_usage_events`.
async fn llm_usage_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM api_usage_events WHERE source = 'llm'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// An unknown-but-plausible term resolves via the LLM, persisting a `source='llm'`
/// alias and recording the call in `api_usage_events`. (Acceptance criteria 1, 2.)
#[tokio::test]
async fn llm_fallback_resolves_unknown_term_and_persists_alias() {
    let pool = setup_pool().await;

    // The fake answers with the target's exact name_en so the cascade exact-matches
    // it to its canonical id.
    let apple_juice_id = id_by_name_en(&pool, LLM_TARGET_NAME).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let llm = Arc::new(FakeLlm {
        answer: Some(LLM_TARGET_NAME.to_string()),
        calls: calls.clone(),
    });
    let state = AppState::new(pool.clone()).with_llm_client(llm);
    let s = seed_seed_household_user_and_list(&pool).await;

    // A novel term with no alias/fuzzy hit → NoMatch → step-3 LLM fallback.
    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", UNKNOWN_TERM)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resolved = poll_entry(&pool, "entry-1", |e| e.resolution_status == "resolved").await;
    assert_eq!(
        resolved.canonical_item_id.as_deref(),
        Some(apple_juice_id.as_str()),
        "LLM-chosen canonical item is bound to the entry"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the LLM was called exactly once"
    );

    // The alias is persisted with source='llm', NORMALISED, language 'en'.
    let (canonical, source, language): (String, String, String) = sqlx::query_as(
        "SELECT canonical_item_id, source, language FROM item_aliases \
         WHERE household_id = ?1 AND lower(alias) = ?2",
    )
    .bind(SEED_HOUSEHOLD)
    .bind(UNKNOWN_TERM)
    .fetch_one(&pool)
    .await
    .expect("an llm alias row exists for the resolved term");
    assert_eq!(canonical, apple_juice_id);
    assert_eq!(source, "llm");
    assert_eq!(language, "en");

    // The call was recorded for cost tracking.
    assert_eq!(
        llm_usage_count(&pool).await,
        1,
        "one llm api_usage_event recorded"
    );
}

/// A second add of the same term resolves via step 1 (alias lookup) with NO second
/// LLM call — the persisted `source='llm'` alias short-circuits the cascade.
/// (Acceptance criterion 3.)
#[tokio::test]
async fn second_add_of_llm_term_resolves_via_alias_without_calling_llm() {
    let pool = setup_pool().await;
    let apple_juice_id = id_by_name_en(&pool, LLM_TARGET_NAME).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let llm = Arc::new(FakeLlm {
        answer: Some(LLM_TARGET_NAME.to_string()),
        calls: calls.clone(),
    });
    let state = AppState::new(pool.clone()).with_llm_client(llm);
    let s = seed_seed_household_user_and_list(&pool).await;

    // First add: resolves via the LLM (call #1), persisting the alias.
    let first = post_mutation(&state, &s, add_item("cm-1", "entry-1", UNKNOWN_TERM)).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    poll_entry(&pool, "entry-1", |e| e.resolution_status == "resolved").await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Second add of the same term: step 1 alias lookup now hits, so the LLM is
    // NOT called again. A distinct constraint set gives it a different
    // effective_constraint_hash, so it resolves standalone (no dedup merge into
    // entry-1) — keeping this test focused on the alias short-circuit.
    let constraints = serde_json::json!({ "brand": "Distinct" });
    let second = post_mutation(
        &state,
        &s,
        add_item_with_constraints("cm-2", "entry-2", UNKNOWN_TERM, Some(constraints)),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CREATED);
    let resolved = poll_entry(&pool, "entry-2", |e| e.resolution_status == "resolved").await;
    assert_eq!(
        resolved.canonical_item_id.as_deref(),
        Some(apple_juice_id.as_str()),
        "the second add bound the same canonical item via the alias"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the second add resolved via step 1 (alias) without a second LLM call"
    );
}

/// An LLM failure (timeout / rate limit) leaves the entry `unresolved` — no panic,
/// no retry storm, and no alias/usage row written. (Acceptance criterion 4.)
#[tokio::test]
async fn llm_failure_leaves_entry_unresolved_without_retry() {
    let pool = setup_pool().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let llm = Arc::new(FailingLlm {
        calls: calls.clone(),
    });
    let state = AppState::new(pool.clone()).with_llm_client(llm);
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", UNKNOWN_TERM)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // A "leche" sentinel posted AFTER proves the failing cascade has run, so a
    // still-unresolved entry-1 is a real LLM failure outcome (not an unscheduled
    // task) — same technique the NoMatch test uses.
    let sentinel = post_mutation(&state, &s, add_item("cm-2", "sentinel", "leche")).await;
    assert_eq!(sentinel.status(), StatusCode::CREATED);
    poll_entry(&pool, "sentinel", |e| e.resolution_status == "resolved").await;

    let entry = entry_state(&pool, "entry-1").await.unwrap();
    assert_eq!(entry.resolution_status, "unresolved");
    assert!(entry.canonical_item_id.is_none());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the LLM was called once and NOT retried"
    );
    // No alias and no usage row written on a failed call.
    let alias_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases WHERE source = 'llm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(alias_rows, 0, "a failed LLM call writes no alias");
    assert_eq!(
        llm_usage_count(&pool).await,
        0,
        "a failed LLM call records no usage"
    );
}

/// With the fallback DISABLED (the default env-built client returns `Disabled`),
/// a NoMatch term stays `unresolved` and the LLM is never consulted.
/// (Acceptance criterion 5.)
#[tokio::test]
async fn disabled_fallback_skips_llm_entirely() {
    let pool = setup_pool().await;
    // AppState::new builds the env-configured client. In the test environment
    // LLM_FALLBACK_ENABLED is unset → disabled → resolve_item returns Disabled.
    let state = AppState::new(pool.clone());
    let s = seed_seed_household_user_and_list(&pool).await;

    let resp = post_mutation(&state, &s, add_item("cm-1", "entry-1", UNKNOWN_TERM)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Sentinel to prove the cascade ran for entry-1.
    let sentinel = post_mutation(&state, &s, add_item("cm-2", "sentinel", "leche")).await;
    assert_eq!(sentinel.status(), StatusCode::CREATED);
    poll_entry(&pool, "sentinel", |e| e.resolution_status == "resolved").await;

    let entry = entry_state(&pool, "entry-1").await.unwrap();
    assert_eq!(
        entry.resolution_status, "unresolved",
        "a disabled fallback leaves the NoMatch entry unresolved"
    );
    assert!(entry.canonical_item_id.is_none());
    assert_eq!(
        llm_usage_count(&pool).await,
        0,
        "a disabled fallback records no usage event"
    );
}
