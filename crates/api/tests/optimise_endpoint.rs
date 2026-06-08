//! Integration tests for `POST /api/optimise` (P4-T1, issue #29).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied. Each test seeds its own
//! catalogue (categories, canonical items, retailer products, offers, matches)
//! and a list with entries, logs a member user in, and POSTs to the endpoint.
//!
//! Coverage (the issue's acceptance criteria + testing expectations):
//!   * a valid list returns the four plans and persists a `recommendation_runs`
//!     row (queried back from the DB);
//!   * `unresolved_items` is populated for entries whose `resolution_status` is
//!     not `resolved` (an ambiguous entry here);
//!   * `plain_language_explanation` names which stores are cheaper for which
//!     categories;
//!   * authentication is enforced (401 without a session; 403 for a non-member);
//!   * a missing list is 404 and `threshold_3 < threshold_2` is 422.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
/// A household we create from scratch (not the seed household) so the catalogue
/// is fully under the test's control.
const HOUSEHOLD: &str = "11111111-1111-1111-1111-111111111111";

const DAIRY_CATEGORY: &str = "cat-dairy";
const BAKERY_CATEGORY: &str = "cat-bakery";
const MILK_ID: &str = "ci-milk";
const BREAD_ID: &str = "ci-bread";

struct Session {
    session: String,
    csrf: String,
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Seed a household, a member user, and log them in.
async fn seed_household_and_user(pool: &SqlitePool, user_id: &str, email: &str) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(HOUSEHOLD)
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
    .bind(HOUSEHOLD)
    .bind(user_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert membership");

    login(pool, email).await
}

/// Seed two categories + two canonical items (Milk in Dairy, Bread in Bakery).
async fn seed_catalogue(pool: &SqlitePool) {
    for (id, name_en, order) in [(DAIRY_CATEGORY, "Dairy", 1), (BAKERY_CATEGORY, "Bakery", 2)] {
        sqlx::query(
            "INSERT INTO categories (id, name_en, name_es, icon, default_order) \
             VALUES (?1, ?2, ?2, 'icon', ?3)",
        )
        .bind(id)
        .bind(name_en)
        .bind(order)
        .execute(pool)
        .await
        .expect("insert category");
    }

    for (id, name, category) in [
        (MILK_ID, "Milk", DAIRY_CATEGORY),
        (BREAD_ID, "Bread", BAKERY_CATEGORY),
    ] {
        sqlx::query(
            "INSERT INTO canonical_items \
                 (id, household_id, name_en, name_es, category_id, \
                  default_constraint_policy, created_at) \
             VALUES (?1, ?2, ?3, ?3, ?4, 'any', ?5)",
        )
        .bind(id)
        .bind(HOUSEHOLD)
        .bind(name)
        .bind(category)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert canonical item");
    }
}

/// Seed one retailer product, an offer at `price_cents`, and an accepted match
/// binding it to `canonical_item_id`.
async fn seed_priced_match(
    pool: &SqlitePool,
    product_id: &str,
    retailer: &str,
    canonical_item_id: &str,
    price_cents: i64,
    snapshot_date: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, pack_count, first_seen_at, last_seen_at) \
         VALUES (?1, 'basketwatch', ?2, ?1, ?1, 1, ?3, ?3)",
    )
    .bind(product_id)
    .bind(retailer)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert product");

    sqlx::query(
        "INSERT INTO product_offers \
             (id, retailer_product_id, price_cents, source_snapshot_date, fetched_at, \
              source_confidence_at_ingest) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'high')",
    )
    .bind(format!("offer-{product_id}"))
    .bind(product_id)
    .bind(price_cents)
    .bind(snapshot_date)
    .bind(db::now_iso())
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
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert match");
}

/// Seed an empty ACTIVE list owned by HOUSEHOLD.
async fn seed_list(pool: &SqlitePool, list_id: &str) {
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES (?1, ?2, 'Weekly', 'active', 0, ?3)",
    )
    .bind(list_id)
    .bind(HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert list");
}

/// Seed one RESOLVED entry bound to a canonical item.
async fn seed_resolved_entry(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    canonical_item_id: &str,
    display: &str,
    added_by: &str,
) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'resolved', ?4, ?4, ?5, 1, 'active', 'default', 0, ?6, ?6)",
    )
    .bind(id)
    .bind(list_id)
    .bind(canonical_item_id)
    .bind(display)
    .bind(added_by)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert resolved entry");
}

/// Seed one AMBIGUOUS entry (no canonical id; non-empty candidate JSON).
async fn seed_ambiguous_entry(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    display: &str,
    added_by: &str,
) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, resolution_status, resolution_candidates_json, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, 'ambiguous', ?3, ?4, ?4, ?5, 1, 'active', ?6, 0, ?7, ?7)",
    )
    .bind(id)
    .bind(list_id)
    .bind(format!("[\"{MILK_ID}\",\"{BREAD_ID}\"]"))
    .bind(display)
    .bind(added_by)
    .bind(format!("unresolved:{id}"))
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert ambiguous entry");
}

/// Seed one UNRESOLVED entry (no canonical id, no candidate JSON).
async fn seed_unresolved_entry(
    pool: &SqlitePool,
    id: &str,
    list_id: &str,
    display: &str,
    added_by: &str,
) {
    sqlx::query(
        "INSERT INTO list_entries \
             (id, list_id, resolution_status, raw_text, display_name, \
              added_by, quantity, status, effective_constraint_hash, last_updated_revision, \
              created_at, updated_at) \
         VALUES (?1, ?2, 'unresolved', ?3, ?3, ?4, 1, 'active', ?5, 0, ?6, ?6)",
    )
    .bind(id)
    .bind(list_id)
    .bind(display)
    .bind(added_by)
    .bind(format!("unresolved:{id}"))
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert unresolved entry");
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
    panic!("cookie {name} not set");
}

/// POST /api/optimise with an authenticated session + CSRF header.
async fn post_optimise(
    pool: &SqlitePool,
    session: &Session,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/optimise")
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
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn optimise_returns_four_plans_persists_run_and_explains() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_catalogue(&pool).await;

    // Aldi is cheapest for Milk (Dairy); Tesco is cheapest for Bread (Bakery).
    seed_priced_match(
        &pool,
        "p-milk-aldi",
        "aldi",
        MILK_ID,
        100,
        Some("2026-06-01"),
    )
    .await;
    seed_priced_match(
        &pool,
        "p-milk-tesco",
        "tesco",
        MILK_ID,
        150,
        Some("2026-06-02"),
    )
    .await;
    seed_priced_match(
        &pool,
        "p-bread-aldi",
        "aldi",
        BREAD_ID,
        220,
        Some("2026-06-01"),
    )
    .await;
    seed_priced_match(
        &pool,
        "p-bread-tesco",
        "tesco",
        BREAD_ID,
        180,
        Some("2026-06-03"),
    )
    .await;

    seed_list(&pool, "list-1").await;
    seed_resolved_entry(&pool, "e-milk", "list-1", MILK_ID, "leche", "user-a").await;
    seed_resolved_entry(&pool, "e-bread", "list-1", BREAD_ID, "pan", "user-a").await;
    // An ambiguous entry must surface in unresolved_items.
    seed_ambiguous_entry(&pool, "e-amb", "list-1", "cream", "user-a").await;

    let resp = post_optimise(&pool, &session, serde_json::json!({ "list_id": "list-1" })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    // Four plans present (non-null): single, two-store, theoretical, recommended.
    assert!(body["best_single"].is_object(), "best_single missing");
    assert!(body["best_two_store"].is_object(), "best_two_store missing");
    assert!(
        body["theoretical_minimum"].is_object(),
        "theoretical_minimum missing"
    );
    assert!(body["recommended"].is_object(), "recommended missing");

    // run_id is returned and the recommendation_runs row is persisted.
    let run_id = body["run_id"].as_str().expect("run_id string");
    let persisted: (String, String, i64, i64) = sqlx::query_as(
        "SELECT recommended_plan, list_id, convenience_threshold_2, convenience_threshold_3 \
         FROM recommendation_runs WHERE id = ?1",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("recommendation_runs row persisted");
    assert_eq!(persisted.1, "list-1");
    // Defaults applied (no thresholds in the request): €10 / €25.
    assert_eq!(persisted.2, 1000);
    assert_eq!(persisted.3, 2500);
    assert!(
        persisted.0 == "single_store" || persisted.0 == "two_store",
        "recommended_plan should be a known kind, got {}",
        persisted.0
    );

    // The persisted plans_json deserialises and carries the four plan keys.
    let plans_json: String =
        sqlx::query_scalar("SELECT plans_json FROM recommendation_runs WHERE id = ?1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("plans_json persisted");
    let plans: Value = serde_json::from_str(&plans_json).expect("plans_json is valid JSON");
    for key in [
        "best_single",
        "best_two_store",
        "theoretical_minimum",
        "recommended",
    ] {
        assert!(
            plans.get(key).is_some(),
            "plans_json missing key {key}: {plans_json}"
        );
    }
    assert!(
        plans["recommended"].is_object(),
        "recommended plan should be persisted non-null"
    );

    // The ambiguous entry is the only unresolved item.
    let unresolved = body["unresolved_items"]
        .as_array()
        .expect("unresolved array");
    assert_eq!(unresolved.len(), 1, "exactly one unresolved item");
    assert_eq!(unresolved[0]["display_text"], "cream");

    // The explanation names stores and categories (cheaper-for phrasing).
    let explanation = body["plain_language_explanation"]
        .as_str()
        .expect("explanation string");
    assert!(explanation.contains("cheaper for"), "got: {explanation}");
    assert!(explanation.contains("Dairy"), "got: {explanation}");
    assert!(explanation.contains("Bakery"), "got: {explanation}");
    assert!(
        explanation.contains("Aldi") && explanation.contains("Tesco"),
        "explanation should name both winning stores, got: {explanation}"
    );

    // offer_snapshot_date is the latest among the recommended plan's offers.
    assert!(
        body["offer_snapshot_date"].is_string(),
        "snapshot date present"
    );
}

#[tokio::test]
async fn optimise_requires_authentication() {
    let pool = setup_pool().await;
    let _ = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    // No session cookie at all.
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/optimise")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "list_id": "list-1" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn optimise_non_member_is_forbidden() {
    let pool = setup_pool().await;
    // Owner seeds the household + list.
    let _owner = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    // A second user in a DIFFERENT household.
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES ('hh-other', 'Other', ?1)")
        .bind(db::now_iso())
        .execute(&pool)
        .await
        .expect("insert other household");
    let hash = buywise_api::auth::hash_password_for_storage(PASSWORD).expect("hash");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES ('user-b', 'b@example.test', ?1, 'user-b', ?2)",
    )
    .bind(&hash)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert user-b");
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES ('hh-other', 'user-b', 'owner', ?1)",
    )
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("insert membership-b");

    let session = login(&pool, "b@example.test").await;
    let resp = post_optimise(&pool, &session, serde_json::json!({ "list_id": "list-1" })).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn optimise_missing_list_is_404() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = post_optimise(&pool, &session, serde_json::json!({ "list_id": "nope" })).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn optimise_threshold3_below_threshold2_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    let resp = post_optimise(
        &pool,
        &session,
        serde_json::json!({
            "list_id": "list-1",
            "convenience_threshold_2": 2000,
            "convenience_threshold_3": 1000
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// An explicit threshold_3 below the *default* threshold_2 (€10) must 422: the
/// ordering guard validates the effective thresholds, not just the explicit pair.
#[tokio::test]
async fn optimise_explicit_threshold3_below_default_threshold2_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    // threshold_3 = €5 (500) < implicit threshold_2 default = €10 (1000).
    let resp = post_optimise(
        &pool,
        &session,
        serde_json::json!({ "list_id": "list-1", "convenience_threshold_3": 500 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// An explicit threshold_2 above the *default* threshold_3 (€25) must 422 too.
#[tokio::test]
async fn optimise_explicit_threshold2_above_default_threshold3_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    // threshold_2 = €50 (5000) > implicit threshold_3 default = €25 (2500).
    let resp = post_optimise(
        &pool,
        &session,
        serde_json::json!({ "list_id": "list-1", "convenience_threshold_2": 5000 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// A negative explicit threshold must 422: thresholds are cents and must be
/// non-negative, since a negative threshold_2 would let an equally-complete,
/// more-expensive two-store split clear the core's `saving > threshold_2` test.
#[tokio::test]
async fn optimise_negative_explicit_threshold_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_list(&pool, "list-1").await;

    // threshold_2 = -€10 (-1000), threshold_3 = 0: ordered, but negative.
    let resp = post_optimise(
        &pool,
        &session,
        serde_json::json!({
            "list_id": "list-1",
            "convenience_threshold_2": -1000,
            "convenience_threshold_3": 0
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// An empty list (nothing resolved, nothing priceable) returns 200 with all-null
/// plans and persists a row with `recommended_plan = "none"`.
#[tokio::test]
async fn optimise_empty_list_persists_recommended_plan_none() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_catalogue(&pool).await;
    seed_list(&pool, "list-1").await;
    // No entries at all.

    let resp = post_optimise(&pool, &session, serde_json::json!({ "list_id": "list-1" })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    assert!(body["best_single"].is_null(), "no single plan");
    assert!(body["recommended"].is_null(), "no recommended plan");
    assert!(
        body["offer_snapshot_date"].is_null(),
        "no snapshot date without a plan"
    );

    let run_id = body["run_id"].as_str().expect("run_id string");
    let (recommended_plan, plans_json): (String, String) = sqlx::query_as(
        "SELECT recommended_plan, plans_json FROM recommendation_runs WHERE id = ?1",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("recommendation_runs row persisted");
    assert_eq!(
        recommended_plan, "none",
        "empty list persists recommended_plan = none"
    );
    // plans_json still deserialises with the four (null) plan keys.
    let plans: Value = serde_json::from_str(&plans_json).expect("plans_json is valid JSON");
    assert!(plans["recommended"].is_null(), "recommended persisted null");
}

/// An `unresolved`-status entry (not just `ambiguous`) surfaces in
/// `unresolved_items`; the criterion is `resolution_status != 'resolved'`.
#[tokio::test]
async fn optimise_unresolved_status_entry_is_unresolved_item() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_catalogue(&pool).await;
    seed_priced_match(
        &pool,
        "p-milk-aldi",
        "aldi",
        MILK_ID,
        100,
        Some("2026-06-01"),
    )
    .await;

    seed_list(&pool, "list-1").await;
    seed_resolved_entry(&pool, "e-milk", "list-1", MILK_ID, "leche", "user-a").await;
    seed_unresolved_entry(&pool, "e-unres", "list-1", "mystery item", "user-a").await;

    let resp = post_optimise(&pool, &session, serde_json::json!({ "list_id": "list-1" })).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;

    let unresolved = body["unresolved_items"]
        .as_array()
        .expect("unresolved array");
    assert_eq!(unresolved.len(), 1, "exactly one unresolved item");
    assert_eq!(unresolved[0]["display_text"], "mystery item");
}
