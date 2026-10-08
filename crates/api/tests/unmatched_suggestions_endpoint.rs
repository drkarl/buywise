//! Integration tests for the unmatched-line suggestion endpoints (follow-up of
//! the Tesco import #107).
//!
//! Drives the real axum router against a private in-memory database. A fake
//! [`Embedder`] supplies invented fixture vectors (no model is run), the batch
//! steps fill the stores, and the endpoints are exercised end to end:
//!
//!   * the list endpoint ranks the invented catalogue and never binds a line;
//!   * the confirm endpoint binds the line and learns an `accepted_match` alias;
//!   * a member of another household is refused.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use buywise_core::embedding::{self, EmbedError, EmbedKind, Embedder};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
const HH_A: &str = "hh-a";
const HH_B: &str = "hh-b";
const MODEL: &str = "test-model";

struct Session {
    session: String,
    csrf: String,
}

/// A fake embedder: a fixed vector per known text, a distinct default otherwise.
struct FakeEmbedder {
    vectors: HashMap<String, Vec<f32>>,
}

#[async_trait]
impl Embedder for FakeEmbedder {
    fn is_enabled(&self) -> bool {
        true
    }

    fn model_id(&self) -> String {
        MODEL.to_string()
    }

    fn dim(&self) -> usize {
        3
    }

    async fn embed(&self, _kind: EmbedKind, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                self.vectors
                    .get(text)
                    .cloned()
                    .unwrap_or_else(|| vec![0.2, 0.2, 0.2])
            })
            .collect())
    }
}

fn fake() -> FakeEmbedder {
    let mut vectors = HashMap::new();
    vectors.insert("Eggs".to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert("Huevos".to_string(), vec![0.98, 0.10, 0.0]);
    vectors.insert("Milk".to_string(), vec![0.0, 1.0, 0.0]);
    vectors.insert("Leche".to_string(), vec![0.0, 0.99, 0.0]);
    vectors.insert(
        "Tesco Free Range Eggs Large Box Of 12".to_string(),
        vec![1.0, 0.05, 0.0],
    );
    FakeEmbedder { vectors }
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn seed_household(
    pool: &SqlitePool,
    household_id: &str,
    user_id: &str,
    email: &str,
) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, ?1, ?2)")
        .bind(household_id)
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

/// Seed a catalogue and one imported transaction with an unmatched line.
async fn seed_catalogue_and_line(pool: &SqlitePool, household_id: &str) {
    sqlx::query(
        "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES ('cat', 'Food', 'Comida', 'x', 1)",
    )
    .execute(pool)
    .await
    .expect("category");
    for (id, en, es) in [("ci-eggs", "Eggs", "Huevos"), ("ci-milk", "Milk", "Leche")] {
        sqlx::query(
            "INSERT INTO canonical_items \
                 (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'cat', 'any', '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(household_id)
        .bind(en)
        .bind(es)
        .execute(pool)
        .await
        .expect("canonical item");
    }
    sqlx::query(
        "INSERT INTO purchase_transactions \
             (id, household_id, retailer, purchase_date, source_kind, source_transaction_id, \
              transaction_identity, identity_kind, fingerprint, currency, total_cents, revision, \
              source_payload, created_at, updated_at) \
         VALUES ('tx-1', ?1, 'tesco', '2026-02-01', 'import', 'tx-1', 'tx-1', 'source_id', 'tx-1', \
                 'EUR', 329, 1, '{}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(household_id)
    .execute(pool)
    .await
    .expect("transaction");
    sqlx::query(
        "INSERT INTO purchase_lines \
             (id, transaction_id, line_ordinal, original_text, name, line_total_cents, \
              match_status, revision, created_at) \
         VALUES ('ln-eggs', 'tx-1', 0, 'Tesco Free Range Eggs Large Box Of 12', \
                 'Tesco Free Range Eggs Large Box Of 12', 329, 'unmatched', 1, '2026-01-01T00:00:00Z')",
    )
    .execute(pool)
    .await
    .expect("line");
}

fn state(pool: &SqlitePool) -> AppState {
    AppState::new(pool.clone()).with_embedder(Arc::new(fake()))
}

async fn get(pool: &SqlitePool, session: &Session, uri: &str) -> axum::response::Response {
    app(state(pool))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn post_json(
    pool: &SqlitePool,
    session: &Session,
    uri: &str,
    body: &Value,
) -> axum::response::Response {
    app(state(pool))
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

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test]
async fn lists_ranked_candidates_and_binds_only_on_confirmation() {
    let pool = setup_pool().await;
    let session = seed_household(&pool, HH_A, "u-a", "a@example.com").await;
    seed_catalogue_and_line(&pool, HH_A).await;

    // Fill the vector stores with the fake embedder (no model is run).
    let embedder = fake();
    embedding::embed_catalogue(&pool, HH_A, &embedder)
        .await
        .expect("embed catalogue");
    embedding::embed_unmatched_names(&pool, HH_A, &embedder)
        .await
        .expect("embed names");

    let resp = get(
        &pool,
        &session,
        &format!("/api/household/{HH_A}/purchases/unmatched-suggestions"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["embedding_enabled"], true);
    let lines = body["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["line_id"], "ln-eggs");
    let candidates = lines[0]["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["canonical_item_id"], "ci-eggs");
    assert_eq!(candidates[0]["name_en"], "Eggs");
    assert!(candidates[0]["score"].as_f64().unwrap() > candidates[1]["score"].as_f64().unwrap());

    // Listing binds nothing and learns no alias.
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM purchase_lines WHERE canonical_item_id IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT COUNT(*) FROM item_aliases WHERE source = 'accepted_match'"
        )
        .await,
        0
    );

    let resp = post_json(
        &pool,
        &session,
        &format!("/api/household/{HH_A}/purchases/unmatched-suggestions/confirm"),
        &json!({ "line_id": "ln-eggs", "canonical_item_id": "ci-eggs" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["canonical_item_id"], "ci-eggs");
    assert_eq!(body["bound_lines"], 1);

    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM purchase_lines WHERE canonical_item_id = 'ci-eggs' AND match_status = 'matched'").await,
        1
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM item_aliases WHERE canonical_item_id = 'ci-eggs' AND source = 'accepted_match'").await,
        1
    );
}

#[tokio::test]
async fn suggestions_are_empty_before_the_batch_step_and_refuse_non_members() {
    let pool = setup_pool().await;
    let session = seed_household(&pool, HH_A, "u-a", "a@example.com").await;
    let other = seed_household(&pool, HH_B, "u-b", "b@example.com").await;
    seed_catalogue_and_line(&pool, HH_A).await;

    // No vectors yet: the line is listed with an empty candidate list.
    let resp = get(
        &pool,
        &session,
        &format!("/api/household/{HH_A}/purchases/unmatched-suggestions"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert!(body["lines"][0]["candidates"]
        .as_array()
        .unwrap()
        .is_empty());

    // A member of another household is refused.
    let resp = get(
        &pool,
        &other,
        &format!("/api/household/{HH_A}/purchases/unmatched-suggestions"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = post_json(
        &pool,
        &other,
        &format!("/api/household/{HH_A}/purchases/unmatched-suggestions/confirm"),
        &json!({ "line_id": "ln-eggs", "canonical_item_id": "ci-eggs" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // An unknown line or item is 404.
    let resp = post_json(
        &pool,
        &session,
        &format!("/api/household/{HH_A}/purchases/unmatched-suggestions/confirm"),
        &json!({ "line_id": "nope", "canonical_item_id": "ci-eggs" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
