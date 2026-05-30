//! Integration test for the P0-T3 seed migration (002_seed.sql).
//!
//! Applies migrations 001 + 002 to an in-memory SQLite database (via the db
//! module's pool helper, which caps private in-memory URLs at one connection so
//! the schema and seed are visible across checkouts) and asserts the row counts,
//! the `leche` -> Milk lookup, and ID uniqueness. The seed inserting at all
//! already proves no `(household_id, lower(alias), language)` unique-index
//! violation, since a duplicate would abort the migration.

use buywise_core::db::{connect, run_migrations};
use sqlx::SqlitePool;

async fn seeded_pool() -> SqlitePool {
    let pool = connect("sqlite::memory:").await.expect("connect in-memory");
    run_migrations(&pool)
        .await
        .expect("run migrations 001 + 002");
    pool
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count query")
}

#[tokio::test]
async fn seed_row_counts_match_acceptance_criteria() {
    let pool = seeded_pool().await;

    assert_eq!(count(&pool, "SELECT COUNT(*) FROM categories").await, 11);
    // 225 ported Shoppy items + nappies_size_4 / nappies_size_5 (P0-T11).
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM canonical_items").await,
        227
    );
    assert!(
        count(&pool, "SELECT COUNT(*) FROM item_aliases").await >= 454,
        "expected at least two aliases per item"
    );
}

#[tokio::test]
async fn leche_resolves_to_milk() {
    let pool = seeded_pool().await;

    let name_en: String = sqlx::query_scalar(
        "SELECT ci.name_en FROM canonical_items ci \
         JOIN item_aliases ia ON ia.canonical_item_id = ci.id \
         WHERE ia.alias = 'leche'",
    )
    .fetch_one(&pool)
    .await
    .expect("leche alias should resolve");

    assert_eq!(name_en, "Milk");
}

#[tokio::test]
async fn canonical_item_ids_are_unique() {
    let pool = seeded_pool().await;

    let total = count(&pool, "SELECT COUNT(*) FROM canonical_items").await;
    let distinct = count(&pool, "SELECT COUNT(DISTINCT id) FROM canonical_items").await;
    assert_eq!(total, distinct, "every canonical_items.id must be unique");
}

#[tokio::test]
async fn all_aliases_are_lowercase_normalised() {
    let pool = seeded_pool().await;

    // No alias should differ from its own lowercase form (proves lowercase).
    let non_lower = count(
        &pool,
        "SELECT COUNT(*) FROM item_aliases WHERE alias != lower(alias)",
    )
    .await;
    assert_eq!(non_lower, 0, "all aliases must be lowercase");

    // No alias should contain leading/trailing/double spaces (whitespace collapse).
    let bad_ws = count(
        &pool,
        "SELECT COUNT(*) FROM item_aliases \
         WHERE alias LIKE ' %' OR alias LIKE '% ' OR alias LIKE '%  %'",
    )
    .await;
    assert_eq!(
        bad_ws, 0,
        "all aliases must be whitespace-collapsed and trimmed"
    );
}
