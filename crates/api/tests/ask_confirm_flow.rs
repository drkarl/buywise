//! End-to-end tests for the ask-policy product-substitution confirm flow (issue
//! #78). Backlog-T1 (#36 / PR #77) only *surfaced* `substitution_policy = 'ask'`
//! items in a "Waiting for your confirmation" section; this flow lets the
//! household actually pick a product and pull the item into the plans.
//!
//! Like `reject_excludes_from_plans.rs`, these tests drive the live axum router
//! (`buywise_api::app`) against a private in-memory SQLite database — no code
//! path between HTTP and SQL is stubbed. They reuse the same login/cookie/seed
//! helper shapes.
//!
//! Coverage (the issue's acceptance criteria):
//!   * AC #1 — the optimise payload carries candidate offer details (id, name,
//!     brand, size, price, retailer) for each ask-gated item, and the item is
//!     held out of every plan;
//!   * AC #3/#4 — persisting a `prefer` choice via `POST /api/match-feedback`
//!     and re-optimising prices the CHOSEN product into the plans and totals, and
//!     drops the item from the confirmation list.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use buywise_api::{app, AppState};
use buywise_core::db;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::SqlitePool;
use tower::ServiceExt; // for `oneshot`

const PASSWORD: &str = "correct horse battery staple";
const HOUSEHOLD_A: &str = "11111111-1111-1111-1111-111111111111";

const DAIRY_CATEGORY: &str = "cat-dairy";
const MILK_ID: &str = "ci-milk";

// Two products matched to Milk. Tesco is cheaper, so a "priced at the chosen
// (dearer) product" assertion proves the household's pick — not a price tie-break
// — drove the plan.
const MILK_TESCO: &str = "rp-milk-tesco";
const MILK_ALDI: &str = "rp-milk-aldi";
const TESCO_PRICE: i64 = 100;
const ALDI_PRICE: i64 = 150;

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

async fn seed_dairy_category(pool: &SqlitePool) {
    sqlx::query(
        "INSERT OR IGNORE INTO categories (id, name_en, name_es, icon, default_order) \
         VALUES (?1, 'Dairy', 'Dairy', 'icon', 1)",
    )
    .bind(DAIRY_CATEGORY)
    .execute(pool)
    .await
    .expect("insert category");
}

/// Seed Milk owned by `household_id` with `default_constraint_policy = 'any'` plus
/// an `item_constraints` row pinning `substitution_policy = 'ask'`, so the item is
/// held back for confirmation.
async fn seed_milk_ask(pool: &SqlitePool, item_id: &str, household_id: &str) {
    sqlx::query(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, \
              default_constraint_policy, created_at) \
         VALUES (?1, ?2, 'Milk', 'Leche', ?3, 'any', ?4)",
    )
    .bind(item_id)
    .bind(household_id)
    .bind(DAIRY_CATEGORY)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert canonical item");

    sqlx::query(
        "INSERT INTO item_constraints (id, canonical_item_id, substitution_policy, updated_at) \
         VALUES (?1, ?2, 'ask', ?3)",
    )
    .bind(format!("ic-{item_id}"))
    .bind(item_id)
    .bind(db::now_iso())
    .execute(pool)
    .await
    .expect("insert item_constraints");
}

/// Seed one (global) retailer product with a brand + SI size, plus a single offer.
#[allow(clippy::too_many_arguments)]
async fn seed_product_with_offer(
    pool: &SqlitePool,
    product_id: &str,
    retailer: &str,
    name: &str,
    brand: &str,
    size_value: f64,
    size_unit: &str,
    price_cents: i64,
) {
    sqlx::query(
        "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, brand, size_value, size_unit, \
              pack_count, first_seen_at, last_seen_at) \
         VALUES (?1, 'basketwatch', ?2, ?1, ?3, ?4, ?5, ?6, 1, ?7, ?7)",
    )
    .bind(product_id)
    .bind(retailer)
    .bind(name)
    .bind(brand)
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

/// POST /api/match-feedback with action `prefer` (the household's product choice).
async fn prefer_match(
    pool: &SqlitePool,
    session: &Session,
    canonical_item_id: &str,
    retailer_product_id: &str,
) {
    let body = serde_json::json!({
        "canonical_item_id": canonical_item_id,
        "retailer_product_id": retailer_product_id,
        "action": "prefer",
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
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "prefer via match-feedback should succeed"
    );
    let out = json_body(resp).await;
    // `prefer` is a stronger accept: status maps to `accepted`, action verbatim.
    assert_eq!(out["status"], "accepted");
    assert_eq!(out["action"], "prefer");
}

/// POST /api/match-feedback with action `reject` (flips the match to `rejected`,
/// which cancels any earlier `prefer` on that match).
async fn reject_match(
    pool: &SqlitePool,
    session: &Session,
    canonical_item_id: &str,
    retailer_product_id: &str,
) {
    let body = serde_json::json!({
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
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "reject via match-feedback should succeed"
    );
    let out = json_body(resp).await;
    assert_eq!(out["status"], "rejected", "match status should be rejected");
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
                .body(Body::from(
                    serde_json::json!({ "list_id": list_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "optimise should succeed");
    json_body(resp).await
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Seed household A with an ask-policy Milk item matched to both products, plus a
/// list containing one resolved Milk entry. Returns the logged-in owner session.
async fn seed_ask_milk_household(pool: &SqlitePool) -> Session {
    seed_household(pool, HOUSEHOLD_A).await;
    let session = seed_member_and_login(pool, "user-a", "a@example.test", HOUSEHOLD_A).await;
    seed_dairy_category(pool).await;
    seed_milk_ask(pool, MILK_ID, HOUSEHOLD_A).await;

    seed_product_with_offer(
        pool,
        MILK_TESCO,
        "tesco",
        "Tesco Milk",
        "Tesco",
        1000.0,
        "ml",
        TESCO_PRICE,
    )
    .await;
    seed_product_with_offer(
        pool,
        MILK_ALDI,
        "aldi",
        "Cleeve Milk",
        "Cleeve",
        2000.0,
        "ml",
        ALDI_PRICE,
    )
    .await;
    seed_accepted_match(pool, HOUSEHOLD_A, MILK_ID, MILK_TESCO).await;
    seed_accepted_match(pool, HOUSEHOLD_A, MILK_ID, MILK_ALDI).await;

    seed_list(pool, "list-a", HOUSEHOLD_A).await;
    seed_resolved_entry(pool, "e-milk", "list-a", MILK_ID, "leche", "user-a").await;

    session
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// AC #1 — an ask-gated item is held out of every plan and surfaced as a
/// needs-confirmation item carrying the candidate offer details (id, name, brand,
/// size, price, retailer), cheapest-first.
#[tokio::test]
async fn ask_item_surfaces_candidate_offers_and_is_not_priced() {
    let pool = setup_pool().await;
    let session = seed_ask_milk_household(&pool).await;

    let body = optimise(&pool, &session, "list-a").await;

    // No plan prices the ask item (it is held back for confirmation).
    assert!(body["best_single"].is_null(), "ask item is not auto-priced");
    assert!(body["recommended"].is_null(), "no recommendation");

    let nc = body["needs_confirmation_items"]
        .as_array()
        .expect("needs_confirmation_items array");
    assert_eq!(nc.len(), 1, "exactly one ask item awaiting confirmation");
    assert_eq!(nc[0]["display_text"], "leche");
    assert_eq!(nc[0]["canonical_item_id"], MILK_ID);
    assert_eq!(nc[0]["candidate_count"], 2);

    let candidates = nc[0]["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2, "both qualifying offers are carried");

    // Cheapest first: Tesco (€1.00) before Aldi (€1.50).
    assert_eq!(candidates[0]["retailer_product_id"], MILK_TESCO);
    assert_eq!(candidates[0]["name"], "Tesco Milk");
    assert_eq!(candidates[0]["brand"], "Tesco");
    assert_eq!(candidates[0]["size_text"], "1000ml");
    assert_eq!(candidates[0]["price_cents"], TESCO_PRICE);
    assert_eq!(candidates[0]["retailer"], "tesco");

    assert_eq!(candidates[1]["retailer_product_id"], MILK_ALDI);
    assert_eq!(candidates[1]["brand"], "Cleeve");
    assert_eq!(candidates[1]["price_cents"], ALDI_PRICE);
    assert_eq!(candidates[1]["retailer"], "aldi");
}

/// AC #3/#4 — persisting a `prefer` choice and re-optimising prices the CHOSEN
/// product into the plans/totals (even when it is the dearer option) and removes
/// the item from the confirmation list.
#[tokio::test]
async fn preferring_a_product_prices_it_and_clears_confirmation() {
    let pool = setup_pool().await;
    let session = seed_ask_milk_household(&pool).await;

    // Before: the item is unpriced and awaiting confirmation.
    let before = optimise(&pool, &session, "list-a").await;
    assert_eq!(
        before["needs_confirmation_items"]
            .as_array()
            .expect("array")
            .len(),
        1
    );
    assert!(before["recommended"].is_null());

    // The household confirms the DEARER Aldi product through the prefer path.
    prefer_match(&pool, &session, MILK_ID, MILK_ALDI).await;

    // After: the item is no longer awaiting confirmation, and it is priced at the
    // chosen Aldi product (€1.50), not the cheaper Tesco one.
    let after = optimise(&pool, &session, "list-a").await;
    assert!(
        after["needs_confirmation_items"]
            .as_array()
            .expect("array")
            .is_empty(),
        "a confirmed ask item leaves the confirmation list"
    );

    let recommended = &after["recommended"]["plan"];
    let lines = recommended["lines"].as_array().expect("lines");
    assert_eq!(
        lines.len(),
        1,
        "exactly one priced line (Milk via the choice)"
    );
    assert_eq!(lines[0]["retailer_product_id"], MILK_ALDI);
    assert_eq!(lines[0]["retailer"], "aldi");
    assert_eq!(
        recommended["total_cents"].as_i64().expect("total"),
        ALDI_PRICE,
        "total reflects the chosen product, not the cheaper Tesco one"
    );

    assert!(
        after["no_valid_offer_items"]
            .as_array()
            .expect("array")
            .is_empty(),
        "the confirmed item is priceable, so it is not a no-valid-offer item"
    );
}

/// Regression: a `reject` after a `prefer` on the SAME product cancels the
/// confirmation. `preferred_product_for_item` only honours a `prefer` whose
/// underlying match is still `accepted` (the `pm.status = 'accepted'` join clause),
/// so flipping that match to `rejected` must drop the confirmation and the ask item
/// must fall back to awaiting a choice — here it surfaces only the surviving Tesco
/// candidate, with the rejected Aldi product excluded from both the plans and the
/// chooser.
#[tokio::test]
async fn rejecting_a_preferred_product_falls_back_to_needs_confirmation() {
    let pool = setup_pool().await;
    let session = seed_ask_milk_household(&pool).await;

    // Confirm Aldi (the dearer product): it is now priced into the plans.
    prefer_match(&pool, &session, MILK_ID, MILK_ALDI).await;
    let confirmed = optimise(&pool, &session, "list-a").await;
    assert!(
        confirmed["needs_confirmation_items"]
            .as_array()
            .expect("array")
            .is_empty(),
        "the confirmed item is priced, not awaiting confirmation"
    );
    assert_eq!(
        confirmed["recommended"]["plan"]["lines"][0]["retailer_product_id"],
        MILK_ALDI,
    );

    // The household then rejects that very product.
    reject_match(&pool, &session, MILK_ID, MILK_ALDI).await;

    // Re-optimise: the prefer no longer applies (its match is rejected), so the
    // item falls back to needs-confirmation and is held out of every plan.
    let after = optimise(&pool, &session, "list-a").await;
    assert!(
        after["best_single"].is_null(),
        "the item is held back again, so no plan prices it"
    );
    assert!(after["recommended"].is_null(), "no recommendation");

    let nc = after["needs_confirmation_items"]
        .as_array()
        .expect("needs_confirmation_items array");
    assert_eq!(nc.len(), 1, "the ask item awaits a choice again");
    assert_eq!(nc[0]["canonical_item_id"], MILK_ID);

    // The rejected Aldi product is gone from the chooser; only Tesco survives.
    let candidates = nc[0]["candidates"].as_array().expect("candidates array");
    assert_eq!(
        candidates.len(),
        1,
        "the rejected product is excluded from the chooser"
    );
    assert_eq!(candidates[0]["retailer_product_id"], MILK_TESCO);
    assert_eq!(nc[0]["candidate_count"], 1);
}
