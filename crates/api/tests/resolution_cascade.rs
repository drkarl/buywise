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
use buywise_api::sse::SseEvent;
use buywise_api::{app, sse, AppState};
use buywise_core::constraint_hash::effective_constraint_hash;
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

    // The merge bumped the revision once; both touched rows carry it. Backfill from
    // just before that revision must replay BOTH the survivor (quantity 2) and the
    // loser (deleted), mirroring the two live SSE broadcasts.
    let merge_revision = entry_revision(&pool, "entry-1").await;
    let events = sse::backfill_since(&pool, "list-a", merge_revision - 1)
        .await
        .unwrap();
    let at_merge: Vec<&serde_json::Value> = events
        .iter()
        .filter_map(|e| {
            let SseEvent::Mutation {
                server_revision,
                entity,
            } = e;
            (*server_revision == merge_revision).then_some(entity)
        })
        .collect();
    assert_eq!(
        at_merge.len(),
        2,
        "the merge replays two events at its revision (survivor + loser)"
    );

    let survivor = at_merge
        .iter()
        .find(|e| e["status"] == "active")
        .expect("a survivor entity in the replay");
    assert_eq!(survivor["resolution_status"], "resolved");
    assert_eq!(survivor["quantity"], 2, "survivor replays with quantity 2");

    let loser = at_merge
        .iter()
        .find(|e| e["status"] == "deleted")
        .expect("a deleted loser entity in the replay");
    assert!(
        ["entry-1", "entry-2"].contains(&loser["id"].as_str().unwrap()),
        "loser is one of the two adds"
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
