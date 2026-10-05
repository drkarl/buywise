//! Household product-preferences endpoint tests (issue #102).
//!
//! Drives the live axum router (`buywise_api::app`) against a private in-memory
//! SQLite database: the `GET`/`PUT /api/items/{id}/preferences` surface, and the
//! end-to-end "Heinz-only beans" acceptance path (set the policy through the API,
//! then re-run `POST /api/optimise` and assert beans are priced only where a Heinz
//! product exists). No layer is stubbed.
//!
//! Coverage:
//!   * policy + constraints round-trip through `PUT` then `GET`;
//!   * household scoping — another household's item is a 404, never leaked;
//!   * boundary validation (brand policy without brand, empty allow-set, size
//!     without basis) — missing data is visible, never silently weakened;
//!   * rejected products stay excluded after a re-optimise, and the preferences
//!     read surfaces the rejected match + the `no_usable_matches` warning;
//!   * Heinz-only beans kept out of a store with no Heinz product.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
const HOUSEHOLD_A: &str = "11111111-1111-1111-1111-111111111111";
const HOUSEHOLD_B: &str = "22222222-2222-2222-2222-222222222222";

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

async fn seed_canonical_item(
    pool: &SqlitePool,
    item_id: &str,
    household_id: &str,
    name: &str,
    policy: &str,
) {
    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(name)
    .bind(PANTRY_CATEGORY)
    .bind(policy)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");
}

async fn seed_product_with_offer(
    pool: &SqlitePool,
    product_id: &str,
    retailer: &str,
    brand: &str,
    price_cents: i64,
) {
    sqlx::query(
        "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, brand, pack_count, first_seen_at, last_seen_at) \
         VALUES (?1, 'basketwatch', ?2, ?1, ?1, ?3, 1, ?4, ?4)",
    )
    .bind(product_id)
    .bind(retailer)
    .bind(brand)
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

async fn seed_accepted_match(
    pool: &SqlitePool,
    household_id: &str,
    canonical_item_id: &str,
    product_id: &str,
) {
    sqlx::query(
        "INSERT INTO product_matches \
             (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, 'accepted', 'system', ?5, ?5)",
    )
    .bind(format!("match-{household_id}-{product_id}"))
    .bind(household_id)
    .bind(canonical_item_id)
    .bind(product_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert match");
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

/// GET /api/items/{id}/preferences, returning (status, body).
async fn get_preferences(
    pool: &SqlitePool,
    session: &Session,
    item_id: &str,
) -> (StatusCode, Value) {
    let resp = app(AppState::new(pool.clone()))
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

/// PUT /api/items/{id}/preferences, returning (status, body).
async fn put_preferences(
    pool: &SqlitePool,
    session: &Session,
    item_id: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/items/{item_id}/preferences"))
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

async fn reject_match(
    pool: &SqlitePool,
    session: &Session,
    canonical_item_id: &str,
    retailer_product_id: &str,
) {
    let body = json!({
        "canonical_item_id": canonical_item_id,
        "retailer_product_id": retailer_product_id,
        "action": "reject",
    });
    let resp = app(AppState::new(pool.clone()))
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
    assert_eq!(resp.status(), StatusCode::CREATED);
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

/// Seed household A with beans matched to a Heinz product (Tesco) and an
/// own-brand product (Aldi), plus a list with one resolved beans entry.
async fn seed_beans_household(pool: &SqlitePool) -> Session {
    seed_household(pool, HOUSEHOLD_A).await;
    let session = seed_member_and_login(pool, "user-a", "a@example.test", HOUSEHOLD_A).await;
    seed_pantry_category(pool).await;
    seed_canonical_item(pool, BEANS_ID, HOUSEHOLD_A, "Baked beans", "any").await;

    seed_product_with_offer(pool, BEANS_TESCO_HEINZ, "tesco", "Heinz", TESCO_PRICE).await;
    seed_product_with_offer(pool, BEANS_ALDI_OWN, "aldi", "Aldi", ALDI_PRICE).await;
    seed_accepted_match(pool, HOUSEHOLD_A, BEANS_ID, BEANS_TESCO_HEINZ).await;
    seed_accepted_match(pool, HOUSEHOLD_A, BEANS_ID, BEANS_ALDI_OWN).await;

    seed_list(pool, "list-a", HOUSEHOLD_A).await;
    seed_resolved_entry(pool, "e-beans", "list-a", BEANS_ID, "beans", "user-a").await;

    session
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A brand policy, its brand, a preferred size and a substitution policy all
/// round-trip: `PUT` persists them and `GET` reads the same values back, along
/// with the item's matches.
#[tokio::test]
async fn policy_and_constraints_round_trip() {
    let pool = setup_pool().await;
    let session = seed_beans_household(&pool).await;

    // Defaults: no constraint row → permissive `any`.
    let (status, before) = get_preferences(&pool, &session, BEANS_ID).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["default_constraint_policy"], "any");
    assert_eq!(before["constraints"]["substitution_policy"], "any");
    assert!(before["constraints"]["brand"].is_null());
    assert_eq!(before["matches"].as_array().expect("matches").len(), 2);

    let (status, after) = put_preferences(
        &pool,
        &session,
        BEANS_ID,
        json!({
            "default_constraint_policy": "brand",
            "brand": "Heinz",
            "allowed_brands": [],
            "excluded_brands": ["Branston"],
            "preferred_size_value": 415.0,
            "preferred_size_basis": "g",
            "substitution_policy": "ask"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "valid preferences should save");
    assert_eq!(after["default_constraint_policy"], "brand");
    assert_eq!(after["constraints"]["brand"], "Heinz");
    assert_eq!(after["constraints"]["excluded_brands"][0], "Branston");
    assert_eq!(after["constraints"]["preferred_size_value"], 415.0);
    assert_eq!(after["constraints"]["preferred_size_basis"], "g");
    assert_eq!(after["constraints"]["substitution_policy"], "ask");

    // Read back through a fresh GET: the stored state is what the optimiser will see.
    let (status, reloaded) = get_preferences(&pool, &session, BEANS_ID).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reloaded["default_constraint_policy"], "brand");
    assert_eq!(reloaded["constraints"]["brand"], "Heinz");
    assert_eq!(reloaded["constraints"]["substitution_policy"], "ask");

    // The policy is also visible on the catalogue read path (`GET /api/items`).
    let items = app(AppState::new(pool.clone()))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/items")
                .header(header::COOKIE, format!("session={}", session.session))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let items = json_body(items).await;
    let bean_item = items["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["id"] == BEANS_ID)
        .expect("beans item present");
    assert_eq!(bean_item["default_constraint_policy"], "brand");
}

/// Another household's canonical item is a 404 for both verbs — the same answer as
/// an unknown id, so a caller cannot probe another household's catalogue.
#[tokio::test]
async fn other_households_item_is_not_found() {
    let pool = setup_pool().await;

    // Household A owns the item.
    seed_household(&pool, HOUSEHOLD_A).await;
    seed_member_and_login(&pool, "user-a", "a@example.test", HOUSEHOLD_A).await;
    seed_pantry_category(&pool).await;
    seed_canonical_item(&pool, BEANS_ID, HOUSEHOLD_A, "Baked beans", "any").await;

    // Household B's member must not see or change it.
    seed_household(&pool, HOUSEHOLD_B).await;
    let session_b = seed_member_and_login(&pool, "user-b", "b@example.test", HOUSEHOLD_B).await;

    let (status, _) = get_preferences(&pool, &session_b, BEANS_ID).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "other household GET → 404");

    let (status, _) = put_preferences(
        &pool,
        &session_b,
        BEANS_ID,
        json!({
            "default_constraint_policy": "any",
            "allowed_brands": [],
            "excluded_brands": [],
            "substitution_policy": "any"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "other household PUT → 404");
}

/// A preference set the optimiser could not honour is rejected at the boundary, so
/// missing data is never silently stored.
#[tokio::test]
async fn inconsistent_preferences_are_422() {
    let pool = setup_pool().await;
    let session = seed_beans_household(&pool).await;

    let brand_without_brand = json!({
        "default_constraint_policy": "brand",
        "allowed_brands": [],
        "excluded_brands": [],
        "substitution_policy": "any"
    });
    let (status, _) = put_preferences(&pool, &session, BEANS_ID, brand_without_brand).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let allowed_set_empty = json!({
        "default_constraint_policy": "allowed_set",
        "allowed_brands": [],
        "excluded_brands": [],
        "substitution_policy": "any"
    });
    let (status, _) = put_preferences(&pool, &session, BEANS_ID, allowed_set_empty).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let size_without_basis = json!({
        "default_constraint_policy": "any",
        "allowed_brands": [],
        "excluded_brands": [],
        "preferred_size_value": 415.0,
        "substitution_policy": "any"
    });
    let (status, _) = put_preferences(&pool, &session, BEANS_ID, size_without_basis).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// The headline acceptance path: setting "Heinz only" on beans from the API keeps
/// beans out of every store without a Heinz product on the next optimisation.
#[tokio::test]
async fn heinz_only_beans_excludes_stores_without_heinz() {
    let pool = setup_pool().await;
    let session = seed_beans_household(&pool).await;

    // Before the preference: Aldi's own-brand (cheaper) wins.
    let before = optimise(&pool, &session, "list-a").await;
    assert_eq!(
        before["recommended"]["plan"]["total_cents"].as_i64(),
        Some(ALDI_PRICE),
        "Aldi own-brand is cheapest before the brand rule"
    );

    // Set Heinz only, entirely through the API.
    let (status, _) = put_preferences(
        &pool,
        &session,
        BEANS_ID,
        json!({
            "default_constraint_policy": "brand",
            "brand": "Heinz",
            "allowed_brands": [],
            "excluded_brands": [],
            "substitution_policy": "any"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // After: only the Heinz product is priced; Aldi is out of EVERY plan.
    let after = optimise(&pool, &session, "list-a").await;
    let ids = priced_product_ids(&after);
    assert!(
        !ids.iter().any(|id| id == BEANS_ALDI_OWN),
        "Aldi (no Heinz product) must not appear in any plan: {ids:?}"
    );
    assert!(
        ids.iter().all(|id| id == BEANS_TESCO_HEINZ),
        "only the Heinz product is priced across every plan: {ids:?}"
    );
    assert_eq!(
        after["recommended"]["plan"]["total_cents"].as_i64(),
        Some(TESCO_PRICE)
    );
    assert_eq!(after["recommended"]["plan"]["stores"][0], "tesco");
}

/// Rejecting the only qualifying product from the preferences screen (written via
/// the existing match-feedback path) keeps it out of the next optimisation, and the
/// preferences read surfaces the rejection plus the `no_usable_matches` warning.
#[tokio::test]
async fn rejected_product_stays_excluded_after_reoptimise() {
    let pool = setup_pool().await;
    let session = seed_beans_household(&pool).await;

    reject_match(&pool, &session, BEANS_ID, BEANS_TESCO_HEINZ).await;

    let after = optimise(&pool, &session, "list-a").await;
    let ids = priced_product_ids(&after);
    assert!(
        !ids.iter().any(|id| id == BEANS_TESCO_HEINZ),
        "rejected product must not appear in any plan: {ids:?}"
    );
    assert!(ids.iter().any(|id| id == BEANS_ALDI_OWN), "Aldi survives");

    // Now reject the survivor too: the item is a no-valid-offer entry (reason
    // no_matches), and the preferences read warns that nothing is usable.
    reject_match(&pool, &session, BEANS_ID, BEANS_ALDI_OWN).await;

    let (status, prefs) = get_preferences(&pool, &session, BEANS_ID).await;
    assert_eq!(status, StatusCode::OK);
    let rejected = prefs["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .filter(|m| m["status"] == "rejected")
        .count();
    assert_eq!(rejected, 2, "both matches read back as rejected");
    assert!(
        prefs["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|w| w == "no_usable_matches"),
        "no_usable_matches warning is surfaced: {}",
        prefs["warnings"]
    );

    let body = optimise(&pool, &session, "list-a").await;
    let no_offer = body["no_valid_offer_items"]
        .as_array()
        .expect("no_valid_offer_items");
    assert_eq!(no_offer.len(), 1);
    assert_eq!(no_offer[0]["reason"], "no_matches");
    // The warning carries the canonical id so the UI can link straight to the
    // preferences screen that fixes it.
    assert_eq!(no_offer[0]["canonical_item_id"], BEANS_ID);
}
