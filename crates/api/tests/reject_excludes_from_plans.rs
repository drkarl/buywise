//! Regression tests: rejected matches are excluded from optimiser plans after the
//! Phase 5 `match_feedback` write path (P5-T2, issue #33).
//!
//! The optimiser only ever loads `status IN ('accepted', 'suggested')` candidates
//! (P0-T10, `db::accepted_or_suggested_matches`), so rejected products are excluded
//! at the source. These tests pin that exclusion *end to end through the real HTTP
//! surface*: they drive the live axum router (`buywise_api::app`) against a private
//! in-memory SQLite database, reject a match via `POST /api/match-feedback`
//! (P5-T1), then re-run `POST /api/optimise` and assert the rejected product is
//! gone — no code path in between is stubbed.
//!
//! Coverage (the issue's acceptance criteria + testing expectation):
//!   * AC #1 — rejecting one of two products for an item drops it from every plan,
//!     and the plan total then reflects the surviving product's price (the testing
//!     expectation, exactly);
//!   * AC #2 — rejecting *all* products for a resolved canonical item surfaces it
//!     in `no_valid_offer_items` (reason `no_matches`), NOT `unresolved_items`
//!     (which is reserved for entries with no canonical id at all);
//!   * AC #3 — a different household's rejection of the *same* (global) retailer
//!     product never touches this household's plans (matches are per-household).
//!
//! The fixtures mirror `optimise_endpoint.rs` and `match_feedback_endpoint.rs`
//! (same login/cookie helpers, same seed shapes) rather than inventing new ones.

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
const HOUSEHOLD_B: &str = "22222222-2222-2222-2222-222222222222";

const DAIRY_CATEGORY: &str = "cat-dairy";
const MILK_ID: &str = "ci-milk";

/// The two retailer products both matched to Milk. Tesco is the cheaper one, so a
/// surviving-product assertion that names Aldi's price proves the rejection (not a
/// price tie-break) is what changed the plan.
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

/// Seed a household with the given id.
async fn seed_household(pool: &SqlitePool, household_id: &str) {
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Test HH', ?2)")
        .bind(household_id)
        .bind(db::now_iso())
        .execute(pool)
        .await
        .expect("insert household");
}

/// Seed a user, make them a member of `household_id`, and log them in.
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

/// Seed the Dairy category (idempotent across households).
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

/// Seed one canonical item (Milk) owned by `household_id`.
async fn seed_milk(pool: &SqlitePool, item_id: &str, household_id: &str) {
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
}

/// Seed one (global) retailer product plus a single offer at `price_cents`.
/// Retailer products are not household-scoped; only the *match* is.
async fn seed_product_with_offer(
    pool: &SqlitePool,
    product_id: &str,
    retailer: &str,
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

/// Seed an `accepted` match binding `product_id` to `canonical_item_id` for
/// `household_id`. The match id is keyed by household so the same product can be
/// matched independently in two households (the isolation test relies on this).
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

/// Seed an empty ACTIVE list owned by `household_id`.
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

/// POST /api/match-feedback (reject) for one (item, product) pair.
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

/// POST /api/optimise for `list_id` and return the parsed JSON body (asserting 200).
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

/// Collect every `retailer_product_id` priced across all four plans of an optimise
/// response (so a rejected product is proven absent everywhere, not just in one).
fn priced_product_ids(body: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for plan_key in [
        "best_single",
        "best_two_store",
        "theoretical_minimum",
        "recommended",
    ] {
        let plan = &body[plan_key];
        // `recommended` nests the plan one level down under `plan`.
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

/// Seed household A with Milk matched to BOTH the Tesco and Aldi products, plus a
/// list containing one resolved Milk entry. Returns the logged-in owner session.
async fn seed_household_a_with_two_milk_products(pool: &SqlitePool) -> Session {
    seed_household(pool, HOUSEHOLD_A).await;
    let session = seed_member_and_login(pool, "user-a", "a@example.test", HOUSEHOLD_A).await;
    seed_dairy_category(pool).await;
    seed_milk(pool, MILK_ID, HOUSEHOLD_A).await;

    seed_product_with_offer(pool, MILK_TESCO, "tesco", TESCO_PRICE).await;
    seed_product_with_offer(pool, MILK_ALDI, "aldi", ALDI_PRICE).await;
    seed_accepted_match(pool, HOUSEHOLD_A, MILK_ID, MILK_TESCO).await;
    seed_accepted_match(pool, HOUSEHOLD_A, MILK_ID, MILK_ALDI).await;

    seed_list(pool, "list-a", HOUSEHOLD_A).await;
    seed_resolved_entry(pool, "e-milk", "list-a", MILK_ID, "leche", "user-a").await;

    session
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// AC #1 — rejecting one of two matched products drops it from every plan, and the
/// recommended plan's total then reflects the surviving product's price.
///
/// Before the rejection, Tesco (cheaper, €1.00) wins. After Tesco is rejected, only
/// Aldi (€1.50) remains, so the surviving line — and the recommended total — must be
/// Aldi's price. This is the testing expectation verbatim.
#[tokio::test]
async fn rejecting_one_product_excludes_it_and_total_reflects_survivor() {
    let pool = setup_pool().await;
    let session = seed_household_a_with_two_milk_products(&pool).await;

    // Sanity: before any rejection, the cheaper Tesco product is the priced one.
    let before = optimise(&pool, &session, "list-a").await;
    let before_ids = priced_product_ids(&before);
    assert!(
        before_ids.iter().any(|id| id == MILK_TESCO),
        "Tesco should be priced before rejection: {before_ids:?}"
    );
    assert_eq!(
        before["recommended"]["plan"]["total_cents"]
            .as_i64()
            .expect("recommended total"),
        TESCO_PRICE,
        "cheaper Tesco product drives the total before rejection"
    );

    // Reject the Tesco match through the Phase 5 write path.
    reject_match(&pool, &session, MILK_ID, MILK_TESCO).await;

    // Re-run: Tesco is gone from EVERY plan, Aldi survives, and the total is Aldi's.
    let after = optimise(&pool, &session, "list-a").await;
    let after_ids = priced_product_ids(&after);
    assert!(
        !after_ids.iter().any(|id| id == MILK_TESCO),
        "rejected Tesco product must not appear in any plan: {after_ids:?}"
    );
    assert!(
        after_ids.iter().any(|id| id == MILK_ALDI),
        "surviving Aldi product must still be priced: {after_ids:?}"
    );

    let recommended = &after["recommended"]["plan"];
    assert_eq!(
        recommended["total_cents"].as_i64().expect("total"),
        ALDI_PRICE,
        "total must reflect the surviving Aldi product's price"
    );
    let lines = recommended["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 1, "exactly one priced line (Milk via Aldi)");
    assert_eq!(lines[0]["retailer_product_id"], MILK_ALDI);
    assert_eq!(lines[0]["retailer"], "aldi");

    // The item is fully priced, so it is NOT a no-valid-offer item.
    assert!(
        after["no_valid_offer_items"]
            .as_array()
            .expect("no_valid_offer_items array")
            .is_empty(),
        "Milk is still priceable via Aldi, so no_valid_offer_items is empty"
    );
}

/// AC #2 — rejecting ALL products for a resolved canonical item makes it surface in
/// `no_valid_offer_items` (reason `no_matches`), and NOT in `unresolved_items`.
///
/// The distinction matters: `unresolved_items` is for entries with no canonical id
/// at all (clarification needed), whereas a resolved item whose every match was
/// rejected is a *priceability* failure, not a resolution failure.
#[tokio::test]
async fn rejecting_all_products_moves_item_to_no_valid_offer_not_unresolved() {
    let pool = setup_pool().await;
    let session = seed_household_a_with_two_milk_products(&pool).await;

    reject_match(&pool, &session, MILK_ID, MILK_TESCO).await;
    reject_match(&pool, &session, MILK_ID, MILK_ALDI).await;

    let body = optimise(&pool, &session, "list-a").await;

    // No plan can price anything now.
    assert!(
        priced_product_ids(&body).is_empty(),
        "no product should be priced once all matches are rejected"
    );
    assert!(body["best_single"].is_null(), "no single-store plan");
    assert!(body["recommended"].is_null(), "no recommendation");

    // The resolved Milk entry is a no-valid-offer item (reason no_matches), NOT
    // an unresolved item.
    let no_offer = body["no_valid_offer_items"]
        .as_array()
        .expect("no_valid_offer_items array");
    assert_eq!(no_offer.len(), 1, "exactly one no-valid-offer item");
    assert_eq!(no_offer[0]["display_text"], "leche");
    assert_eq!(
        no_offer[0]["reason"], "no_matches",
        "all matches rejected ⇒ NoMatches reason"
    );

    assert!(
        body["unresolved_items"]
            .as_array()
            .expect("unresolved_items array")
            .is_empty(),
        "a resolved item with rejected matches must NOT be unresolved"
    );
}

/// AC #3 — household isolation. Two households each match the SAME global retailer
/// products to their own Milk item. Household B rejects the Tesco match; household
/// A's plans are untouched (matches are keyed by household).
#[tokio::test]
async fn other_household_rejection_does_not_affect_this_households_plans() {
    let pool = setup_pool().await;

    // Household A: Milk matched to both products; one resolved Milk entry.
    let session_a = seed_household_a_with_two_milk_products(&pool).await;

    // Household B: its own Milk canonical item, matched to the SAME global products.
    seed_household(&pool, HOUSEHOLD_B).await;
    let session_b = seed_member_and_login(&pool, "user-b", "b@example.test", HOUSEHOLD_B).await;
    let milk_b = "ci-milk-b";
    seed_milk(&pool, milk_b, HOUSEHOLD_B).await;
    seed_accepted_match(&pool, HOUSEHOLD_B, milk_b, MILK_TESCO).await;
    seed_accepted_match(&pool, HOUSEHOLD_B, milk_b, MILK_ALDI).await;

    // Household B rejects its OWN match to the Tesco product.
    reject_match(&pool, &session_b, milk_b, MILK_TESCO).await;

    // Household A's plans are unaffected: Tesco still wins, total still €1.00.
    let body_a = optimise(&pool, &session_a, "list-a").await;
    let ids_a = priced_product_ids(&body_a);
    assert!(
        ids_a.iter().any(|id| id == MILK_TESCO),
        "household A's Tesco match must survive household B's rejection: {ids_a:?}"
    );
    assert_eq!(
        body_a["recommended"]["plan"]["total_cents"]
            .as_i64()
            .expect("total"),
        TESCO_PRICE,
        "household A's total is unchanged by household B's rejection"
    );
    assert!(
        body_a["no_valid_offer_items"]
            .as_array()
            .expect("array")
            .is_empty(),
        "household A still prices Milk"
    );

    // And the rejection DID land for household B: its product_matches row is rejected.
    let status_b: Option<String> = sqlx::query_scalar(
        "SELECT status FROM product_matches \
         WHERE household_id = ?1 AND canonical_item_id = ?2 AND retailer_product_id = ?3",
    )
    .bind(HOUSEHOLD_B)
    .bind(milk_b)
    .bind(MILK_TESCO)
    .fetch_optional(&pool)
    .await
    .expect("query B status");
    assert_eq!(
        status_b.as_deref(),
        Some("rejected"),
        "household B's own match should be rejected"
    );
}
