//! Integration tests for the Phase 1 migrations (003 + 004).
//!
//! These mirror the harness in `db.rs`: build an in-memory pool via
//! `db::connect` (which enables `PRAGMA foreign_keys`) and then apply every
//! embedded migration (001..004) in lexical order via `db::run_migrations`.

use buywise_core::db;
use sqlx::SqlitePool;

/// A fresh in-memory pool with all four migrations applied (001..004).
async fn migrated_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("connect in-memory");
    db::run_migrations(&pool).await.expect("run migrations");
    pool
}

/// Seed the minimum rows needed to insert a `list_entries` row that satisfies
/// every foreign key, so the only thing under test is the CHECK constraints.
async fn seed_list_prereqs(pool: &SqlitePool) {
    // The default household is already seeded by 004; add a list + a canonical
    // item we can bind to.
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, created_at, updated_at)
         VALUES ('list-1', '00000000-0000-0000-0000-000000000001', 'Test', 't', 't')",
    )
    .execute(pool)
    .await
    .unwrap();

    // canonical_items references categories(id); reuse a category seeded by 002
    // and stamp the row under the default household seeded by 004.
    sqlx::query(
        "INSERT INTO canonical_items
            (id, household_id, name_en, name_es, category_id,
             default_constraint_policy, created_at)
         SELECT 'ci-1', '00000000-0000-0000-0000-000000000001', 'Milk', 'Leche',
                c.id, 'any', 't'
         FROM categories c LIMIT 1",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn all_migrations_apply_and_seed_55_store_category_orders() {
    let pool = migrated_pool().await;

    // The 55-row cross join depends on categories having exactly 11 rows; assert
    // the category count too so a change to the 002 seed fails here, not silently.
    let categories: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM categories")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        categories, 11,
        "expected 11 seeded categories from 002_seed.sql"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM store_category_orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 55, "expected 5 retailers x 11 categories = 55 rows");

    // The default household must match the Phase 0 canonical-items household.
    let household: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM households WHERE id = '00000000-0000-0000-0000-000000000001'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(household, 1);
}

/// Helper: a `list_entries` INSERT with the given column values must be rejected
/// by a CHECK constraint.
async fn assert_entry_insert_fails(pool: &SqlitePool, values_sql: &str, what: &str) {
    let sql = format!(
        "INSERT INTO list_entries
            (id, list_id, raw_text, status, canonical_item_id,
             resolution_candidates_json, effective_constraint_hash,
             created_at, updated_at)
         VALUES {values_sql}"
    );
    let err = sqlx::query(&sql).execute(pool).await;
    assert!(err.is_err(), "expected CHECK to reject: {what}");
}

#[tokio::test]
async fn list_entries_check_constraints_reject_invalid_states() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    // (1) status enum: an unknown status is rejected.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'bogus', NULL, NULL, 'h', 't', 't')",
        "status not in enum",
    )
    .await;

    // (2) resolved must bind a canonical_item_id.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'resolved', NULL, NULL, 'h', 't', 't')",
        "resolved without canonical_item_id",
    )
    .await;

    // (3) a bound canonical_item_id requires status = resolved.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'unresolved', 'ci-1', NULL, 'h', 't', 't')",
        "canonical_item_id set but status not resolved",
    )
    .await;

    // (4) unresolved must have a NULL candidate list.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'unresolved', NULL, '[\"a\"]', 'h', 't', 't')",
        "unresolved with non-null candidates",
    )
    .await;

    // (5a) ambiguous must have a non-null candidate list.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'ambiguous', NULL, NULL, 'h', 't', 't')",
        "ambiguous with null candidates",
    )
    .await;

    // (5b) ambiguous candidate list must be non-empty.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'ambiguous', NULL, '[]', 'h', 't', 't')",
        "ambiguous with empty candidate array",
    )
    .await;

    // (5c) ambiguous candidate list must be well-formed JSON: json_valid() folded
    // into the CHECK turns malformed JSON into a clean CHECK violation rather than
    // an opaque json_array_length() parse error.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'ambiguous', NULL, 'not-json', 'h', 't', 't')",
        "ambiguous with malformed JSON candidates",
    )
    .await;

    // (6) resolved must clear the candidate list.
    assert_entry_insert_fails(
        &pool,
        "('e', 'list-1', 'x', 'resolved', 'ci-1', '[\"a\"]', 'h', 't', 't')",
        "resolved with leftover candidates",
    )
    .await;
}

#[tokio::test]
async fn list_entries_accepts_each_valid_state() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    // unresolved: no canonical item, no candidates.
    sqlx::query(
        "INSERT INTO list_entries
            (id, list_id, raw_text, status, canonical_item_id,
             resolution_candidates_json, effective_constraint_hash,
             created_at, updated_at)
         VALUES ('u', 'list-1', 'milk', 'unresolved', NULL, NULL, 'h1', 't', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // ambiguous: non-empty candidate list, no bound item.
    sqlx::query(
        "INSERT INTO list_entries
            (id, list_id, raw_text, status, canonical_item_id,
             resolution_candidates_json, effective_constraint_hash,
             created_at, updated_at)
         VALUES ('a', 'list-1', 'milk', 'ambiguous', NULL, '[\"ci-1\",\"ci-2\"]', 'h2', 't', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // resolved: bound item, no candidates.
    sqlx::query(
        "INSERT INTO list_entries
            (id, list_id, raw_text, status, canonical_item_id,
             resolution_candidates_json, effective_constraint_hash,
             created_at, updated_at)
         VALUES ('r', 'list-1', 'milk', 'resolved', 'ci-1', NULL, 'h3', 't', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn active_dedup_unique_index_blocks_duplicate_resolved_entries() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    let insert = |id: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO list_entries
                    (id, list_id, raw_text, status, canonical_item_id,
                     resolution_candidates_json, effective_constraint_hash,
                     entry_status, created_at, updated_at)
                 VALUES (?, 'list-1', 'milk', 'resolved', 'ci-1', NULL, 'same', 'active', 't', 't')",
            )
            .bind(id)
            .execute(&pool)
            .await
        }
    };

    insert("d1").await.unwrap();
    // Same (list, canonical_item, constraint hash) while active -> rejected.
    assert!(insert("d2").await.is_err());
}

#[tokio::test]
async fn mutations_client_mutation_id_is_unique() {
    let pool = migrated_pool().await;

    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at)
         VALUES ('user-1', 'a@b.c', 'A', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let insert = |id: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO mutations
                    (id, client_mutation_id, household_id, user_id,
                     mutation_type, payload_json, applied_at, created_at)
                 VALUES (?, 'cmid-1', '00000000-0000-0000-0000-000000000001',
                         'user-1', 'add_entry', '{}', 't', 't')",
            )
            .bind(id)
            .execute(&pool)
            .await
        }
    };

    insert("m1").await.unwrap();
    assert!(
        insert("m2").await.is_err(),
        "duplicate client_mutation_id must fail"
    );
}
