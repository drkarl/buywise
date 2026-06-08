//! Integration tests for `GET`/`PATCH /api/settings` (P4-T2, issue #30).
//!
//! Drives the real axum router (`buywise_api::app`) against a private in-memory
//! SQLite database with the full migrations applied (001..005). Each test seeds
//! its own household + member user, logs in, and exercises the settings surface.
//!
//! Coverage (the issue's acceptance criteria + testing expectations):
//!   * `GET /api/settings` returns the household's seeded thresholds (€10 / €25);
//!   * `PATCH /api/settings` updates one or both thresholds and persists them;
//!   * a `PATCH` whose effective `threshold_3 < threshold_2` is 422;
//!   * `POST /api/optimise` without explicit thresholds uses the household's
//!     stored values, and the recommendation flips at the convenience boundary
//!     when the stored threshold_2 is moved across the two-store saving.

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
/// and its settings row are fully under the test's control.
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

/// Seed a household, a member user, and log them in. Also inserts the default
/// `household_settings` row (migration 005 only back-fills households that exist
/// *at migration time*; a test-created household must seed its own row to mirror
/// the production "seed settings when the household is created" contract).
async fn seed_household_and_user(pool: &SqlitePool, user_id: &str, email: &str) -> Session {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(HOUSEHOLD)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");

    sqlx::query(
        "INSERT INTO household_settings \
             (household_id, convenience_threshold_2_cents, convenience_threshold_3_cents, updated_at) \
         VALUES (?1, 1000, 2500, ?2)",
    )
    .bind(HOUSEHOLD)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert household_settings");

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
    .bind("2026-06-01")
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

/// GET /api/settings with an authenticated session.
async fn get_settings(pool: &SqlitePool, session: &Session) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/settings")
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// PATCH /api/settings with an authenticated session + CSRF header.
async fn patch_settings(
    pool: &SqlitePool,
    session: &Session,
    body: Value,
) -> axum::response::Response {
    app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/settings")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                .header("X-CSRF-Token", &session.csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
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

/// `GET /api/settings` returns the household's seeded defaults (€10 / €25).
#[tokio::test]
async fn get_settings_returns_seeded_defaults() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = get_settings(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["convenience_threshold_2_cents"], 1000);
    assert_eq!(body["convenience_threshold_3_cents"], 2500);
}

/// `GET /api/settings` without a session is 401 (extractor-enforced).
#[tokio::test]
async fn get_settings_requires_authentication() {
    let pool = setup_pool().await;
    let _ = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// `GET /api/settings` falls back to the documented €10 / €25 defaults when the
/// household has no `household_settings` row (the defensive no-row branch).
#[tokio::test]
async fn get_settings_no_row_returns_defaults() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    // Remove the seeded settings row so the household has none.
    sqlx::query("DELETE FROM household_settings WHERE household_id = ?1")
        .bind(HOUSEHOLD)
        .execute(&pool)
        .await
        .expect("delete settings row");

    let resp = get_settings(&pool, &session).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["convenience_threshold_2_cents"], 1000);
    assert_eq!(body["convenience_threshold_3_cents"], 2500);
}

/// `PATCH /api/settings` without an `X-CSRF-Token` header is 403 (the extractor
/// enforces CSRF on mutating verbs), even with a valid session cookie.
#[tokio::test]
async fn patch_settings_without_csrf_is_forbidden() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/settings")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, format!("session={}", session.session))
                // Deliberately NO X-CSRF-Token header.
                .body(Body::from(
                    serde_json::json!({ "convenience_threshold_2_cents": 1500 }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// `PATCH /api/settings` updates both thresholds, returns the new values, and
/// persists them (a follow-up `GET` reflects the change).
#[tokio::test]
async fn patch_settings_updates_both_thresholds() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = patch_settings(
        &pool,
        &session,
        serde_json::json!({
            "convenience_threshold_2_cents": 1500,
            "convenience_threshold_3_cents": 3000
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["convenience_threshold_2_cents"], 1500);
    assert_eq!(body["convenience_threshold_3_cents"], 3000);

    // Persisted: a fresh GET returns the updated values.
    let body = json_body(get_settings(&pool, &session).await).await;
    assert_eq!(body["convenience_threshold_2_cents"], 1500);
    assert_eq!(body["convenience_threshold_3_cents"], 3000);
}

/// A partial `PATCH` updates only the provided field and leaves the other as-is.
#[tokio::test]
async fn patch_settings_partial_update_leaves_other_field() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    // Only threshold_2; threshold_3 stays at its seeded €25.
    let resp = patch_settings(
        &pool,
        &session,
        serde_json::json!({ "convenience_threshold_2_cents": 1200 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["convenience_threshold_2_cents"], 1200);
    assert_eq!(body["convenience_threshold_3_cents"], 2500);
}

/// An empty `PATCH` body (no fields) is a no-op 422.
#[tokio::test]
async fn patch_settings_empty_body_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = patch_settings(&pool, &session, serde_json::json!({})).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// A `PATCH` whose effective `threshold_3 < threshold_2` is 422 (consistent with
/// the optimise endpoint's ordering guard).
#[tokio::test]
async fn patch_settings_threshold3_below_threshold2_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    let resp = patch_settings(
        &pool,
        &session,
        serde_json::json!({
            "convenience_threshold_2_cents": 2000,
            "convenience_threshold_3_cents": 1000
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// A partial `PATCH` that raises threshold_2 above the *stored* threshold_3 is
/// 422: the ordering guard validates the effective (post-merge) pair, not just
/// the provided field.
#[tokio::test]
async fn patch_settings_partial_threshold2_above_stored_threshold3_is_422() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;

    // threshold_2 = €30 (3000) > stored threshold_3 default = €25 (2500).
    let resp = patch_settings(
        &pool,
        &session,
        serde_json::json!({ "convenience_threshold_2_cents": 3000 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// The acceptance test: update the household's threshold via `PATCH`, run the
/// optimiser WITHOUT an explicit threshold, and verify the recommendation flips
/// at the convenience boundary.
///
/// Catalogue (two stores, two items):
///   * Milk:  Aldi €1.00 (100),  Tesco €1.50 (150)
///   * Bread: Aldi €2.20 (220),  Tesco €1.80 (180)
/// Best single store: Aldi €3.20 (320). Best two-store split: Milk@Aldi 100 +
/// Bread@Tesco 180 = €2.80 (280). The two-store saving is exactly €0.40 (40c).
/// The optimiser recommends two-store only when the saving STRICTLY exceeds the
/// stored threshold_2, so:
///   * stored threshold_2 = 30c (< 40c saving) => two_store recommended;
///   * stored threshold_2 = 50c (> 40c saving) => single_store recommended.
#[tokio::test]
async fn optimise_uses_stored_threshold_and_flips_at_boundary() {
    let pool = setup_pool().await;
    let session = seed_household_and_user(&pool, "user-a", "a@example.test").await;
    seed_catalogue(&pool).await;

    seed_priced_match(&pool, "p-milk-aldi", "aldi", MILK_ID, 100).await;
    seed_priced_match(&pool, "p-milk-tesco", "tesco", MILK_ID, 150).await;
    seed_priced_match(&pool, "p-bread-aldi", "aldi", BREAD_ID, 220).await;
    seed_priced_match(&pool, "p-bread-tesco", "tesco", BREAD_ID, 180).await;

    seed_list(&pool, "list-1").await;
    seed_resolved_entry(&pool, "e-milk", "list-1", MILK_ID, "leche", "user-a").await;
    seed_resolved_entry(&pool, "e-bread", "list-1", BREAD_ID, "pan", "user-a").await;

    // Below the €0.40 two-store saving: the split is recommended.
    let resp = patch_settings(
        &pool,
        &session,
        serde_json::json!({ "convenience_threshold_2_cents": 30 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body =
        json_body(post_optimise(&pool, &session, serde_json::json!({ "list_id": "list-1" })).await)
            .await;
    assert_eq!(
        body["recommended"]["kind"], "two_store",
        "stored threshold below the saving must recommend the two-store split: {body:#}"
    );
    // The run persisted the *stored* effective threshold, not the core default.
    let run_id = body["run_id"].as_str().expect("run_id");
    let stored_2: i64 =
        sqlx::query_scalar("SELECT convenience_threshold_2 FROM recommendation_runs WHERE id = ?1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("run row");
    assert_eq!(stored_2, 30, "optimise persisted the stored threshold_2");

    // Above the €0.40 saving: the single store is recommended (the flip).
    let resp = patch_settings(
        &pool,
        &session,
        serde_json::json!({ "convenience_threshold_2_cents": 50 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body =
        json_body(post_optimise(&pool, &session, serde_json::json!({ "list_id": "list-1" })).await)
            .await;
    assert_eq!(
        body["recommended"]["kind"], "single_store",
        "stored threshold above the saving must recommend the single store: {body:#}"
    );

    // An explicit request threshold still overrides the stored value: with a tiny
    // explicit threshold_2 the split is recommended again despite the stored 50c.
    let body = json_body(
        post_optimise(
            &pool,
            &session,
            serde_json::json!({ "list_id": "list-1", "convenience_threshold_2": 10 }),
        )
        .await,
    )
    .await;
    assert_eq!(
        body["recommended"]["kind"], "two_store",
        "an explicit request threshold overrides the stored value: {body:#}"
    );
}
