//! Integration tests for the Phase 1 migrations (003 + 004).
//!
//! These mirror the harness in `db.rs`: build an in-memory pool via
//! `db::connect` (which enables `PRAGMA foreign_keys`) and then apply every
//! embedded migration (001..004) in lexical order via `db::run_migrations`.
//!
//! The schema under test is the canonical one in docs/implementation-plan.md
//! (P1-T1): `list_entries` carries TWO orthogonal state columns —
//! `resolution_status` (the six CHECK invariants) and `status` (item lifecycle,
//! used by the dedup index predicate `WHERE status = 'active'`).

use buywise_core::db;
use sqlx::SqlitePool;

const HOUSEHOLD: &str = "00000000-0000-0000-0000-000000000001";

/// A fresh in-memory pool with all four migrations applied (001..004).
async fn migrated_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:")
        .await
        .expect("connect in-memory");
    db::run_migrations(&pool).await.expect("run migrations");
    pool
}

/// Seed the minimum rows needed to insert a `list_entries` row that satisfies
/// every foreign key (user for `added_by`, a list, a canonical item), so the
/// only thing under test is the CHECK constraints.
async fn seed_list_prereqs(pool: &SqlitePool) {
    // A user for list_entries.added_by / bought_by.
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at)
         VALUES ('user-1', 'a@b.c', 'argon2id$dummy', 'A', 't')",
    )
    .execute(pool)
    .await
    .unwrap();

    // A list under the default household (seeded by 004).
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, created_at)
         VALUES ('list-1', ?, 'Test', 't')",
    )
    .bind(HOUSEHOLD)
    .execute(pool)
    .await
    .unwrap();

    // canonical_items references categories(id); reuse a category seeded by 002
    // and stamp the row under the default household seeded by 004.
    sqlx::query(
        "INSERT INTO canonical_items
            (id, household_id, name_en, name_es, category_id,
             default_constraint_policy, created_at)
         SELECT 'ci-1', ?, 'Milk', 'Leche', c.id, 'any', 't'
         FROM categories c LIMIT 1",
    )
    .bind(HOUSEHOLD)
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
    assert_eq!(count, 55, "expected 5 stores x 11 categories = 55 rows");

    // The default household must match the Phase 0 canonical-items household.
    let household: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM households WHERE id = ?")
        .bind(HOUSEHOLD)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(household, 1);
}

/// Migration 005 back-fills one `household_settings` row per existing household
/// with the documented €10 / €25 defaults (P4-T2).
#[tokio::test]
async fn migration_005_backfills_household_settings_defaults() {
    let pool = migrated_pool().await;

    let (threshold_2, threshold_3): (i64, i64) = sqlx::query_as(
        "SELECT convenience_threshold_2_cents, convenience_threshold_3_cents \
         FROM household_settings WHERE household_id = ?",
    )
    .bind(HOUSEHOLD)
    .fetch_one(&pool)
    .await
    .expect("seed household has a back-filled settings row");
    assert_eq!(threshold_2, 1000, "default threshold_2 is €10.00");
    assert_eq!(threshold_3, 2500, "default threshold_3 is €25.00");
}

/// Helper: a `list_entries` INSERT with the given (resolution_status,
/// canonical_item_id, resolution_candidates_json) triple must be rejected by a
/// CHECK constraint. All other columns are filled with valid values so the only
/// thing that can fail is the resolution invariant under test.
async fn assert_entry_insert_fails(
    pool: &SqlitePool,
    id: &str,
    resolution_status: &str,
    canonical_item_id: Option<&str>,
    candidates: Option<&str>,
    what: &str,
) {
    let err = sqlx::query(
        "INSERT INTO list_entries
            (id, list_id, canonical_item_id, resolution_status,
             resolution_candidates_json, raw_text, display_name, added_by,
             effective_constraint_hash, created_at, updated_at)
         VALUES (?, 'list-1', ?, ?, ?, 'milk', 'Milk', 'user-1', ?, 't', 't')",
    )
    .bind(id)
    .bind(canonical_item_id)
    .bind(resolution_status)
    .bind(candidates)
    .bind(format!("h-{id}"))
    .execute(pool)
    .await;
    assert!(err.is_err(), "expected CHECK to reject: {what}");
}

#[tokio::test]
async fn list_entries_resolution_checks_reject_invalid_states() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    // (0) resolution_status enum: an unknown value is rejected.
    assert_entry_insert_fails(
        &pool,
        "e0",
        "bogus",
        None,
        None,
        "resolution_status not in enum",
    )
    .await;

    // (1) resolved => canonical_item_id IS NOT NULL.
    assert_entry_insert_fails(
        &pool,
        "e1",
        "resolved",
        None,
        None,
        "resolved without canonical_item_id",
    )
    .await;

    // (2) canonical_item_id NOT NULL => resolved.
    assert_entry_insert_fails(
        &pool,
        "e2",
        "unresolved",
        Some("ci-1"),
        None,
        "canonical_item_id set but not resolved",
    )
    .await;

    // (3) unresolved => resolution_candidates_json IS NULL.
    assert_entry_insert_fails(
        &pool,
        "e3",
        "unresolved",
        None,
        Some("[\"a\"]"),
        "unresolved with non-null candidates",
    )
    .await;

    // (4a) ambiguous => candidates IS NOT NULL.
    assert_entry_insert_fails(
        &pool,
        "e4a",
        "ambiguous",
        None,
        None,
        "ambiguous with null candidates",
    )
    .await;

    // (4b) ambiguous => candidates is non-empty.
    assert_entry_insert_fails(
        &pool,
        "e4b",
        "ambiguous",
        None,
        Some("[]"),
        "ambiguous with empty candidate array",
    )
    .await;

    // (4c) ambiguous => candidates is well-formed JSON (json_valid folded into
    // the CHECK turns malformed JSON into a clean violation, not an opaque
    // json_array_length() runtime error).
    assert_entry_insert_fails(
        &pool,
        "e4c",
        "ambiguous",
        None,
        Some("not-json"),
        "ambiguous with malformed JSON candidates",
    )
    .await;

    // (5) resolved => candidates IS NULL.
    assert_entry_insert_fails(
        &pool,
        "e5",
        "resolved",
        Some("ci-1"),
        Some("[\"a\"]"),
        "resolved with leftover candidates",
    )
    .await;
}

#[tokio::test]
async fn list_entries_status_lifecycle_check_rejects_unknown_values() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    // `status` is the item lifecycle: only active | bought | skipped | deleted.
    let bad = sqlx::query(
        "INSERT INTO list_entries
            (id, list_id, canonical_item_id, resolution_status,
             resolution_candidates_json, raw_text, display_name, added_by,
             status, effective_constraint_hash, created_at, updated_at)
         VALUES ('s-bad', 'list-1', NULL, 'unresolved', NULL, 'milk', 'Milk',
                 'user-1', 'removed', 'h', 't', 't')",
    )
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "status outside (active,bought,skipped,deleted) must fail"
    );

    // All four valid lifecycle values are accepted (on unresolved rows, so the
    // dedup index — which only covers resolved+active — is not involved).
    for status in ["active", "bought", "skipped", "deleted"] {
        sqlx::query(
            "INSERT INTO list_entries
                (id, list_id, canonical_item_id, resolution_status,
                 resolution_candidates_json, raw_text, display_name, added_by,
                 status, effective_constraint_hash, created_at, updated_at)
             VALUES (?, 'list-1', NULL, 'unresolved', NULL, 'milk', 'Milk',
                     'user-1', ?, ?, 't', 't')",
        )
        .bind(format!("s-{status}"))
        .bind(status)
        .bind(format!("h-{status}"))
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("status '{status}' should be accepted: {e}"));
    }
}

#[tokio::test]
async fn list_entries_accepts_each_valid_resolution_state() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    let insert = |id: &'static str,
                  resolution_status: &'static str,
                  canonical: Option<&'static str>,
                  candidates: Option<&'static str>,
                  hash: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO list_entries
                    (id, list_id, canonical_item_id, resolution_status,
                     resolution_candidates_json, raw_text, display_name,
                     added_by, effective_constraint_hash, created_at, updated_at)
                 VALUES (?, 'list-1', ?, ?, ?, 'milk', 'Milk', 'user-1', ?, 't', 't')",
            )
            .bind(id)
            .bind(canonical)
            .bind(resolution_status)
            .bind(candidates)
            .bind(hash)
            .execute(&pool)
            .await
        }
    };

    insert("u", "unresolved", None, None, "h1").await.unwrap();
    insert("a", "ambiguous", None, Some("[\"ci-1\",\"ci-2\"]"), "h2")
        .await
        .unwrap();
    insert("r", "resolved", Some("ci-1"), None, "h3")
        .await
        .unwrap();
}

#[tokio::test]
async fn active_dedup_index_blocks_duplicate_resolved_actives_but_allows_inactive() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    let insert = |id: &'static str, status: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO list_entries
                    (id, list_id, canonical_item_id, resolution_status,
                     resolution_candidates_json, raw_text, display_name,
                     added_by, status, effective_constraint_hash,
                     created_at, updated_at)
                 VALUES (?, 'list-1', 'ci-1', 'resolved', NULL, 'milk', 'Milk',
                         'user-1', ?, 'same', 't', 't')",
            )
            .bind(id)
            .bind(status)
            .execute(&pool)
            .await
        }
    };

    // First active resolved row: OK.
    insert("d1", "active").await.unwrap();
    // Second active row with the same (list, item, hash): rejected by the index.
    assert!(
        insert("d2", "active").await.is_err(),
        "duplicate active resolved entry must be rejected"
    );
    // A non-active duplicate (bought) is OUTSIDE the partial index predicate and
    // must be allowed — this proves the predicate is correctly scoped.
    insert("d3", "bought")
        .await
        .expect("non-active duplicate must be allowed by the partial index");
}

#[tokio::test]
async fn mutations_client_mutation_id_is_unique() {
    let pool = migrated_pool().await;

    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at)
         VALUES ('user-1', 'a@b.c', 'argon2id$dummy', 'A', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, created_at)
         VALUES ('list-1', ?, 'Test', 't')",
    )
    .bind(HOUSEHOLD)
    .execute(&pool)
    .await
    .unwrap();

    let insert = |id: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO mutations
                    (id, client_mutation_id, device_id, user_id, list_id,
                     entity_type, entity_id, operation_type, payload,
                     base_revision, created_at_client, received_at_server)
                 VALUES (?, 'cmid-1', 'dev-1', 'user-1', 'list-1',
                         'list_entry', 'e-1', 'add_item', '{}', 0, 't', 't')",
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

#[tokio::test]
async fn mutations_operation_type_check_rejects_unknown() {
    let pool = migrated_pool().await;
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at)
         VALUES ('user-1', 'a@b.c', 'argon2id$dummy', 'A', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO shopping_lists (id, household_id, name, created_at)
         VALUES ('list-1', ?, 'Test', 't')",
    )
    .bind(HOUSEHOLD)
    .execute(&pool)
    .await
    .unwrap();

    let bad = sqlx::query(
        "INSERT INTO mutations
            (id, client_mutation_id, device_id, user_id, list_id,
             entity_type, entity_id, operation_type, payload,
             base_revision, created_at_client, received_at_server)
         VALUES ('m1', 'cmid-x', 'dev-1', 'user-1', 'list-1',
                 'list_entry', 'e-1', 'frobnicate', '{}', 0, 't', 't')",
    )
    .execute(&pool)
    .await;
    assert!(bad.is_err(), "unknown operation_type must be rejected");
}

/// Migration 008 widens the operation_type CHECK to include `set_quantity`
/// (issue #60) by rebuilding the table; this asserts the new value is accepted
/// after the rebuild (and so the constraint was recreated, not dropped).
#[tokio::test]
async fn mutations_operation_type_check_accepts_set_quantity() {
    let pool = migrated_pool().await;
    // Reuse the shared seed helper for the user-1/list-1 prerequisites the FK
    // constraints need (it also seeds an unused canonical item, which is harmless).
    seed_list_prereqs(&pool).await;

    let ok = sqlx::query(
        "INSERT INTO mutations
            (id, client_mutation_id, device_id, user_id, list_id,
             entity_type, entity_id, operation_type, payload,
             base_revision, created_at_client, received_at_server)
         VALUES ('m1', 'cmid-q', 'dev-1', 'user-1', 'list-1',
                 'list_entry', 'e-1', 'set_quantity', '{\"quantity\":3}', 0, 't', 't')",
    )
    .execute(&pool)
    .await;
    assert!(
        ok.is_ok(),
        "set_quantity must be accepted after migration 008"
    );
}
