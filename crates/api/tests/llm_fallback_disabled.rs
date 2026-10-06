//! Acceptance test for the documented external-AI switch (issue #119): with the
//! optional LLM fallback OFF — the shipped default
//! (`LLM_FALLBACK_ENABLED` unset/false, see `deploy/README.md` and
//! `docs/data-flows.md`) — the shared list keeps working end to end.
//!
//! It drives the real axum router over a private in-memory database and proves
//! the three things the household must not lose by turning external processing
//! off:
//!   * `POST /api/mutations` still adds list entries,
//!   * server-side resolution still resolves an aliased term and still leaves an
//!     unknown term for the user to clarify,
//!   * `POST /api/optimise` still prices the list and recommends a plan.
//!
//! The injected client records every call, so "off" is verified rather than
//! assumed: the counter must stay at zero and no `api_usage_events` row may be
//! written (`source = 'llm'` is the usage marker the fallback writes).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::llm::{LlmClient, LlmError};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
/// A household created from scratch so its catalogue is fully under test control.
const HOUSEHOLD: &str = "22222222-2222-2222-2222-222222222222";
const CATEGORY: &str = "cat-dairy";
const LIST_ID: &str = "list-1";
const MILK_ID: &str = "ci-milk";
const BREAD_ID: &str = "ci-bread";
/// A term with no alias, no near match and no plausible LLM answer.
const UNKNOWN_TERM: &str = "xyzqrst_unknown_term";

/// Stands in for the shipped default (`LLM_FALLBACK_ENABLED=false`): it reports
/// itself disabled, and counts any call so a call that must not happen is
/// visible in the assertions.
struct RecordingDisabledLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmClient for RecordingDisabledLlm {
    fn is_enabled(&self) -> bool {
        false
    }

    async fn resolve_item(
        &self,
        _raw_text: &str,
        _candidate_names: &[String],
    ) -> Result<Option<String>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(LlmError::Disabled)
    }

    async fn rank_candidates(
        &self,
        _item_description: &str,
        _candidate_labels: &[String],
    ) -> Result<Option<Vec<String>>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(LlmError::Disabled)
    }
}

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed the household, a member, the list, a two-item catalogue with aliases,
/// and an accepted match + offer per item so `POST /api/optimise` can price them.
async fn seed_fixture(pool: &SqlitePool, email: &str) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(HOUSEHOLD)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");

    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES ('user-a', ?1, ?2, 'user-a', ?3)",
    )
    .bind(email)
    .bind(&hash)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert user");
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, 'user-a', 'owner', ?2)",
    )
    .bind(HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'Weekly', 'active', 0, ?3)",
    )
    .bind(LIST_ID)
    .bind(HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");

    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES (?1, 'Dairy', 'Lácteos', 'icon', 1)",
    )
    .bind(CATEGORY)
    .execute(pool)
    .await
    .expect("insert category");

    for (id, name) in [(MILK_ID, "Milk"), (BREAD_ID, "Bread")] {
        sqlx::query(
            "INSERT INTO canonical_items \
                 (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
             VALUES (?1, ?2, ?3, ?3, ?4, 'any', ?5)",
        )
        .bind(id)
        .bind(HOUSEHOLD)
        .bind(name)
        .bind(CATEGORY)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert canonical item");
        // An alias is what step 1 of the cascade resolves, with no LLM involved.
        sqlx::query(
            "INSERT INTO item_aliases \
                 (id, household_id, canonical_item_id, alias, language, source, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'en', 'manual', ?5)",
        )
        .bind(format!("alias-{id}"))
        .bind(HOUSEHOLD)
        .bind(id)
        .bind(name.to_ascii_lowercase())
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert alias");
    }

    // One priced, accepted match per item so the optimiser has something to
    // compare.
    seed_priced_match(pool, "p-milk-tesco", "tesco", MILK_ID, 150).await;
    seed_priced_match(pool, "p-milk-aldi", "aldi", MILK_ID, 100).await;
    seed_priced_match(pool, "p-bread-tesco", "tesco", BREAD_ID, 220).await;

    login(pool, email).await
}

async fn seed_priced_match(
    pool: &SqlitePool,
    product_id: &str,
    retailer: &str,
    canonical_item_id: &str,
    price_cents: i64,
) {
    let now = db::now_iso();
    sqlx::query(
        "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, pack_count, first_seen_at, last_seen_at) \
         VALUES (?1, 'basketwatch', ?2, ?1, ?1, 1, ?3, ?3)",
    )
    .bind(product_id)
    .bind(retailer)
    .bind(&now)
    .execute(pool)
    .await
    .expect("insert product");
    sqlx::query(
        "INSERT INTO product_offers \
             (id, retailer_product_id, price_cents, source_snapshot_date, fetched_at, \
              source_confidence_at_ingest) \
         VALUES (?1, ?2, ?3, '2026-06-01', ?4, 'high')",
    )
    .bind(format!("offer-{product_id}"))
    .bind(product_id)
    .bind(price_cents)
    .bind(&now)
    .execute(pool)
    .await
    .expect("insert offer");
    sqlx::query(
        "INSERT INTO product_matches \
             (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, 'accepted', 'system', ?5, ?5)",
    )
    .bind(format!("match-{product_id}"))
    .bind(HOUSEHOLD)
    .bind(canonical_item_id)
    .bind(product_id)
    .bind(&now)
    .execute(pool)
    .await
    .expect("insert match");
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

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn post_json(
    state: &AppState,
    session: &Session,
    uri: &str,
    body: Value,
) -> axum::response::Response {
    app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
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

fn add_item(client_mutation_id: &str, entity_id: &str, raw_text: &str) -> Value {
    json!({
        "client_mutation_id": client_mutation_id,
        "device_id": "device-1",
        "list_id": LIST_ID,
        "base_revision": 0,
        "entity_type": "list_entry",
        "entity_id": entity_id,
        "operation_type": "add_item",
        "payload": { "raw_text": raw_text, "display_name": raw_text },
        "created_at_client": "2026-06-04T00:00:00.000Z",
    })
}

/// `(resolution_status, canonical_item_id)` for one entry.
async fn entry_state(pool: &SqlitePool, entry_id: &str) -> Option<(String, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT resolution_status, canonical_item_id FROM list_entries WHERE id = ?1",
    )
    .bind(entry_id)
    .fetch_optional(pool)
    .await
    .expect("read entry")
}

/// Wait up to 2s for `entry_id` to satisfy `predicate`, so the assertion does
/// not race the detached resolution task.
async fn wait_for(
    pool: &SqlitePool,
    entry_id: &str,
    predicate: impl Fn(&str, &Option<String>) -> bool,
) -> (String, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some((status, canonical)) = entry_state(pool, entry_id).await {
            if predicate(&status, &canonical) {
                return (status, canonical);
            }
        }
        assert!(
            Instant::now() < deadline,
            "entry {entry_id} did not reach the expected state in 2s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn llm_usage_count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM api_usage_events WHERE source = 'llm'")
        .fetch_one(pool)
        .await
        .expect("count llm usage")
}

#[tokio::test]
async fn list_add_resolution_and_optimise_work_with_the_external_ai_switch_off() {
    let pool = setup_pool().await;
    let session = seed_fixture(&pool, "a@example.test").await;

    let calls = Arc::new(AtomicUsize::new(0));
    let state = AppState::new(pool.clone()).with_llm_client(Arc::new(RecordingDisabledLlm {
        calls: calls.clone(),
    }));

    // 1. List add + resolution: an aliased term still resolves with no LLM.
    let resp = post_json(
        &state,
        &session,
        "/api/mutations",
        add_item("cm-1", "entry-milk", "milk"),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "add_item must be accepted"
    );
    let (status, canonical) = wait_for(&pool, "entry-milk", |s, c| {
        s == "resolved" && c.as_deref() == Some(MILK_ID)
    })
    .await;
    assert_eq!(status, "resolved");
    assert_eq!(canonical.as_deref(), Some(MILK_ID));

    // 2. A second resolvable term, used as the ordering sentinel for step 3.
    let resp = post_json(
        &state,
        &session,
        "/api/mutations",
        add_item("cm-2", "entry-bread", "bread"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    wait_for(&pool, "entry-bread", |s, c| {
        s == "resolved" && c.as_deref() == Some(BREAD_ID)
    })
    .await;

    // 3. An unknown term stays unresolved for the user to clarify — the fallback
    //    that would have guessed is off, and the list still works.
    let resp = post_json(
        &state,
        &session,
        "/api/mutations",
        add_item("cm-3", "entry-unknown", UNKNOWN_TERM),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    // The bread entry resolved *after* the unknown add was queued, so the
    // cascade has run past it; a still-unresolved entry is a real NoMatch.
    let (status, canonical) = wait_for(&pool, "entry-unknown", |s, _| s != "resolved").await;
    assert_eq!(status, "unresolved");
    assert_eq!(canonical, None);

    // 4. Optimisation still prices the resolved list and recommends a plan.
    let resp = post_json(
        &state,
        &session,
        "/api/optimise",
        json!({ "list_id": LIST_ID }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "optimise must still run");
    let body = json_body(resp).await;
    assert!(
        body["best_single"].is_object(),
        "best_single missing: {body}"
    );
    assert!(
        body["recommended"].is_object(),
        "recommended missing: {body}"
    );
    let priced = body["best_single"]["total_cents"]
        .as_i64()
        .expect("best_single total");
    assert!(priced > 0, "the plan must price the resolved entries");
    // The three unresolved-entry checks share one list; only the unknown term is
    // reported as unresolved.
    let unresolved: Vec<&str> = body["unresolved_items"]
        .as_array()
        .expect("unresolved_items")
        .iter()
        .filter_map(|item| item["entry_id"].as_str())
        .collect();
    assert_eq!(unresolved, vec!["entry-unknown"]);

    // 5. Nothing external happened: no LLM call, no usage row.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an off fallback must not call the model (resolution or candidate ranking)"
    );
    assert_eq!(
        llm_usage_count(&pool).await,
        0,
        "no usage event for a call that never happened"
    );
}
