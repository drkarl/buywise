//! Product candidate matching endpoint tests (issue #108).
//!
//! Drives the live axum router (`buywise_api::app`) against a private in-memory
//! SQLite database. Covers the whole in-app matching flow built on top of
//! `GET /api/items/{id}/preferences` (which now also proposes candidates) and
//! `POST /api/match-feedback` (accept / reject / undo):
//!
//!   * candidates are proposed per store with price provenance;
//!   * a rejected candidate does not recur, and `undo` restores it (the
//!     deliberate reset), while the rejection itself stays in the feedback trail;
//!   * `undo` requires an existing decision (404 otherwise) and returns the match
//!     to `pending`;
//!   * matching an unmatched item makes it priced on the next optimisation;
//!   * the LLM ranking seam reorders candidates when enabled and is a no-op by
//!     default.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::llm::{LlmClient, LlmError};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
const HOUSEHOLD: &str = "11111111-1111-1111-1111-111111111111";

const PANTRY_CATEGORY: &str = "cat-pantry";
const BEANS_ID: &str = "ci-beans";

const BEANS_TESCO_HEINZ: &str = "rp-beans-tesco-heinz";
const BEANS_ALDI_OWN: &str = "rp-beans-aldi-own";
const TESCO_PRICE: i64 = 120;
const ALDI_PRICE: i64 = 85;

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn seed_household(pool: &SqlitePool, household_id: &str) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(household_id)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");
}

async fn seed_member_and_login(
    pool: &SqlitePool,
    user_id: &str,
    email: &str,
    household_id: &str,
) -> Session {
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

async fn seed_pantry_category(pool: &SqlitePool) {
    sqlx::query(
        "INSERT OR IGNORE INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES (?1, 'Pantry', 'Pantry', 'icon', 1)",
    )
    .bind(PANTRY_CATEGORY)
    .execute(pool)
    .await
    .expect("insert category");
}

async fn seed_canonical_item(pool: &SqlitePool, item_id: &str, name: &str, policy: &str) {
    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
    )
    .bind(item_id)
    .bind(HOUSEHOLD)
    .bind(name)
    .bind(PANTRY_CATEGORY)
    .bind(policy)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");
}

/// Insert a priced product with an explicit name/brand/size so candidate ranking
/// has something to score.
#[allow(clippy::too_many_arguments)]
async fn seed_product(
    pool: &SqlitePool,
    product_id: &str,
    retailer: &str,
    name: &str,
    brand: &str,
    pack_size_text: &str,
    size_value: f64,
    size_unit: &str,
    price_cents: i64,
) {
    sqlx::query(
        "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, brand, pack_size_text, size_value, \
              size_unit, pack_count, first_seen_at, last_seen_at) \
         VALUES (?1, 'basketwatch', ?2, ?1, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
    )
    .bind(product_id)
    .bind(retailer)
    .bind(name)
    .bind(brand)
    .bind(pack_size_text)
    .bind(size_value)
    .bind(size_unit)
    .bind(db::now_iso())
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
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert offer");
}

async fn seed_list(pool: &SqlitePool, list_id: &str, household_id: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'Weekly', 'active', 0, ?3)",
    )
    .bind(list_id)
    .bind(household_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

async fn seed_resolved_entry(pool: &SqlitePool, id: &str, list_id: &str, added_by: &str) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'resolved', 'beans', 'beans', ?4, 1, 'active', 'default', 0, ?5, ?5)",
    )
    .bind(id)
    .bind(list_id)
    .bind(BEANS_ID)
    .bind(added_by)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert resolved entry");
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
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).unwrap()
}

async fn get_preferences_with_state(
    state: AppState,
    session: &Session,
    item_id: &str,
) -> (StatusCode, Value) {
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/items/{item_id}/preferences"))
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, json_body(resp).await)
}

async fn get_preferences(
    pool: &SqlitePool,
    session: &Session,
    item_id: &str,
) -> (StatusCode, Value) {
    get_preferences_with_state(AppState::new(pool.clone()), session, item_id).await
}

async fn post_feedback_with_state(
    state: AppState,
    session: &Session,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/match-feedback")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, json_body(resp).await)
}

async fn post_feedback(
    pool: &SqlitePool,
    session: &Session,
    canonical_item_id: &str,
    retailer_product_id: &str,
    action: &str,
) -> (StatusCode, Value) {
    post_feedback_with_state(
        AppState::new(pool.clone()),
        session,
        json!({
            "canonical_item_id": canonical_item_id,
            "retailer_product_id": retailer_product_id,
            "action": action,
        }),
    )
    .await
}

async fn optimise(pool: &SqlitePool, session: &Session, list_id: &str) -> Value {
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/optimise")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(json!({ "list_id": list_id }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "optimise should succeed");
    json_body(resp).await
}

/// Every `retailer_product_id` priced across all four plans.
fn priced_product_ids(body: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for plan_key in [
        "best_single",
        "best_two_store",
        "theoretical_minimum",
        "recommended",
    ] {
        let plan = &body[plan_key];
        let lines = if plan_key == "recommended" {
            plan.get("plan").and_then(|p| p.get("lines"))
        } else {
            plan.get("lines")
        };
        if let Some(lines) = lines.and_then(|l| l.as_array()) {
            for line in lines {
                if let Some(id) = line["retailer_product_id"].as_str() {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

fn candidate_ids(body: &Value) -> Vec<String> {
    body["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|c| c["retailer_product_id"].as_str().unwrap().to_string())
        .collect()
}

/// Seed household + item "Baked beans" + one Heinz product at Tesco and one
/// own-brand at Aldi, with NO matches — the item is unmatched and needs proposing.
async fn seed_unmatched_household(pool: &SqlitePool) -> Session {
    seed_household(pool, HOUSEHOLD).await;
    let session = seed_member_and_login(pool, "user-a", "a@example.test", HOUSEHOLD).await;
    seed_pantry_category(pool).await;
    seed_canonical_item(pool, BEANS_ID, "Baked beans", "any").await;
    seed_product(
        pool,
        BEANS_TESCO_HEINZ,
        "tesco",
        "Heinz Baked Beans 415g",
        "Heinz",
        "415g",
        415.0,
        "g",
        TESCO_PRICE,
    )
    .await;
    seed_product(
        pool,
        BEANS_ALDI_OWN,
        "aldi",
        "Aldi Baked Beans 420g",
        "Aldi",
        "420g",
        420.0,
        "g",
        ALDI_PRICE,
    )
    .await;
    session
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An unmatched item is proposed candidates per store, each carrying the product
/// identity and the price provenance the household decides on.
#[tokio::test]
async fn candidates_are_proposed_with_price_provenance() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;

    let (status, body) = get_preferences(&pool, &session, BEANS_ID).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["matches"].as_array().expect("matches").len(), 0);

    let candidates = body["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2, "one candidate per seeded product");

    let ids = candidate_ids(&body);
    assert!(ids.contains(&BEANS_TESCO_HEINZ.to_string()));
    assert!(ids.contains(&BEANS_ALDI_OWN.to_string()));

    for candidate in candidates {
        assert!(candidate["name"].as_str().is_some());
        assert!(candidate["retailer"].as_str().is_some());
        assert_eq!(candidate["offer_snapshot_date"], "2026-06-01");
        assert_eq!(candidate["source_confidence"], "high");
        assert!(candidate["fetched_at"].as_str().is_some());
        assert!(candidate["signals"]
            .as_array()
            .expect("signals")
            .iter()
            .any(|s| s == "name"));
        assert!(candidate["score"].as_i64().expect("score") > 0);
    }

    let heinz = candidates
        .iter()
        .find(|c| c["retailer_product_id"] == BEANS_TESCO_HEINZ)
        .expect("heinz candidate");
    assert_eq!(heinz["price_cents"], TESCO_PRICE);
    assert_eq!(heinz["brand"], "Heinz");
    assert_eq!(heinz["size_text"], "415g");
    assert_eq!(heinz["retailer"], "tesco");
}

/// A rejected candidate is not proposed again, and `undo` (the deliberate reset)
/// brings it back — while the rejection stays in the append-only feedback trail.
#[tokio::test]
async fn rejected_candidate_does_not_recur_without_an_undo() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;

    let (_, before) = get_preferences(&pool, &session, BEANS_ID).await;
    assert!(candidate_ids(&before).contains(&BEANS_TESCO_HEINZ.to_string()));

    let (status, _) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "reject").await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, after_reject) = get_preferences(&pool, &session, BEANS_ID).await;
    let ids = candidate_ids(&after_reject);
    assert!(
        !ids.contains(&BEANS_TESCO_HEINZ.to_string()),
        "a rejected candidate must not recur"
    );
    assert!(ids.contains(&BEANS_ALDI_OWN.to_string()));

    // Deliberate reset: undo returns the product to `pending`, so it is proposed
    // again — and the rejection row is still in history.
    let (status, body) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "undo").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], "pending");

    let (_, after_undo) = get_preferences(&pool, &session, BEANS_ID).await;
    assert!(
        candidate_ids(&after_undo).contains(&BEANS_TESCO_HEINZ.to_string()),
        "undo must restore the candidate"
    );

    let heedless: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM match_feedback WHERE action = 'reject'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let undos: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM match_feedback WHERE action = 'undo'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(heedless, 1, "the rejection is preserved in history");
    assert_eq!(undos, 1);
}

/// `undo` corrects an existing decision to `pending`; undoing a product that was
/// never decided is a 404, never a silent insert.
#[tokio::test]
async fn undo_requires_an_existing_decision() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;

    let (status, _) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "undo").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "accept").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "undo").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], "pending");

    let status_db: String =
        sqlx::query_scalar("SELECT status FROM product_matches WHERE canonical_item_id = ?1")
            .bind(BEANS_ID)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status_db, "pending");
}

/// The headline acceptance path: matching an unmatched item makes it priced on the
/// very next optimisation.
#[tokio::test]
async fn matching_an_unmatched_item_makes_it_priced() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;
    seed_list(&pool, "list-a", HOUSEHOLD).await;
    seed_resolved_entry(&pool, "e-beans", "list-a", "user-a").await;

    // Before matching: resolved but no match → surfaced as no-price, never priced.
    let before = optimise(&pool, &session, "list-a").await;
    assert!(priced_product_ids(&before).is_empty());
    let no_offer = before["no_valid_offer_items"]
        .as_array()
        .expect("no_valid_offer_items");
    assert_eq!(no_offer.len(), 1);
    assert_eq!(no_offer[0]["reason"], "no_matches");

    // Accept the candidate.
    let (status, _) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "accept").await;
    assert_eq!(status, StatusCode::CREATED);

    // After matching: the product is priced.
    let after = optimise(&pool, &session, "list-a").await;
    assert!(
        priced_product_ids(&after).contains(&BEANS_TESCO_HEINZ.to_string()),
        "the accepted match must be priced"
    );
    assert!(after["no_valid_offer_items"]
        .as_array()
        .expect("no_valid_offer_items")
        .is_empty());
}

/// The list snapshot flags a resolved item with no usable match, and clears the
/// flag once a candidate is accepted.
#[tokio::test]
async fn list_snapshot_marks_unmatched_items_until_matched() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;
    seed_list(&pool, "list-a", HOUSEHOLD).await;
    seed_resolved_entry(&pool, "e-beans", "list-a", "user-a").await;

    async fn get_list(pool: &SqlitePool, session: &Session) -> Value {
        let resp = app(AppState::new(pool.clone()))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/list/list-a")
                    .header(header::COOKIE, format!("session={}", session.session))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        json_body(resp).await
    }

    let before = get_list(&pool, &session).await;
    let entry = before["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "e-beans")
        .expect("entry present");
    assert_eq!(entry["needs_match"], true);

    let (status, _) = post_feedback(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ, "accept").await;
    assert_eq!(status, StatusCode::CREATED);

    let after = get_list(&pool, &session).await;
    let entry = after["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "e-beans")
        .expect("entry present");
    assert_eq!(entry["needs_match"], false);
}

/// A fake LLM that reverses the candidate order it is given.
struct ReversingLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmClient for ReversingLlm {
    fn is_enabled(&self) -> bool {
        true
    }

    async fn resolve_item(
        &self,
        _raw_text: &str,
        _candidate_names: &[String],
    ) -> Result<Option<String>, LlmError> {
        Ok(None)
    }

    async fn rank_candidates(
        &self,
        _item_description: &str,
        candidate_labels: &[String],
    ) -> Result<Option<Vec<String>>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(candidate_labels.iter().rev().cloned().collect()))
    }
}

/// With the LLM fallback enabled, ranking reorders the candidates it was given,
/// and the call is recorded for cost tracking. With the default (disabled) client
/// the deterministic order stands.
#[tokio::test]
async fn llm_ranking_reorders_candidates_and_is_recorded() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;

    // Deterministic baseline: both Tesco products tie on name score, so the
    // tie-break is product name ascending.
    seed_product(
        &pool,
        "rp-beans-tesco-zebra",
        "tesco",
        "Zebra Baked Beans 400g",
        "Zebra",
        "400g",
        400.0,
        "g",
        99,
    )
    .await;

    let (_, baseline) = get_preferences(&pool, &session, BEANS_ID).await;
    let baseline_ids = candidate_ids(&baseline);
    // Retailer order (aldi first), then name ascending within Tesco.
    assert_eq!(
        baseline_ids,
        vec![
            BEANS_ALDI_OWN.to_string(),
            BEANS_TESCO_HEINZ.to_string(),
            "rp-beans-tesco-zebra".to_string(),
        ]
    );

    // Enabled fake reverses the whole candidate list.
    let calls = Arc::new(AtomicUsize::new(0));
    let state = AppState::new(pool.clone()).with_llm_client(Arc::new(ReversingLlm {
        calls: calls.clone(),
    }));
    let (status, ranked) = get_preferences_with_state(state, &session, BEANS_ID).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the LLM is asked once");

    let ranked_ids = candidate_ids(&ranked);
    let reversed: Vec<String> = baseline_ids.iter().rev().cloned().collect();
    assert_eq!(
        ranked_ids, reversed,
        "the fake reverses the deterministic order"
    );

    let llm_usage: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_usage_events WHERE source = 'llm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        llm_usage, 1,
        "the ranking call is recorded for cost tracking"
    );
}

/// A fake LLM that reports the fallback disabled: `is_enabled` false. The
/// candidate loader must not call it and must keep the deterministic order.
struct DisabledLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmClient for DisabledLlm {
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
}

#[tokio::test]
async fn disabled_llm_is_never_called() {
    let pool = setup_pool().await;
    let session = seed_unmatched_household(&pool).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let state = AppState::new(pool.clone()).with_llm_client(Arc::new(DisabledLlm {
        calls: calls.clone(),
    }));
    let (status, body) = get_preferences_with_state(state, &session, BEANS_ID).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(body["candidates"].as_array().unwrap().len(), 2);

    let llm_usage: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM api_usage_events WHERE source = 'llm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(llm_usage, 0, "a disabled fallback writes no usage row");
}
