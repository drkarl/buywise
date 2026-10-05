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

/// Migration 011 back-fills one `household_store_settings` row per existing
/// household × retailer, all `active` and not nearby (issue #98), so a household
/// that configures nothing keeps the pre-#98 planning behaviour.
#[tokio::test]
async fn migration_011_backfills_household_store_defaults() {
    let pool = migrated_pool().await;

    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT retailer, status, nearby FROM household_store_settings \
         WHERE household_id = ? ORDER BY retailer",
    )
    .bind(HOUSEHOLD)
    .fetch_all(&pool)
    .await
    .expect("back-filled store settings rows");

    assert_eq!(rows.len(), 5, "one row per retailer");
    let retailers: Vec<&str> = rows.iter().map(|(r, _, _)| r.as_str()).collect();
    assert_eq!(retailers, ["aldi", "dunnes", "lidl", "supervalu", "tesco"]);
    for (retailer, status, nearby) in rows {
        assert_eq!(status, "active", "{retailer} defaults to active");
        assert_eq!(nearby, 0, "{retailer} defaults to not nearby");
    }
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

/// Migration 009 widens the operation_type CHECK to include `move_to_list` by
/// rebuilding the table; this asserts the new value is accepted after the rebuild
/// (and so the constraint was recreated, not dropped).
#[tokio::test]
async fn mutations_operation_type_check_accepts_move_to_list() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    let ok = sqlx::query(
        "INSERT INTO mutations
            (id, client_mutation_id, device_id, user_id, list_id,
             entity_type, entity_id, operation_type, payload,
             base_revision, created_at_client, received_at_server)
         VALUES ('m1', 'cmid-mtl', 'dev-1', 'user-1', 'list-1',
                 'list_entry', 'e-1', 'move_to_list', '{}', 0, 't', 't')",
    )
    .execute(&pool)
    .await;
    assert!(
        ok.is_ok(),
        "move_to_list must be accepted after migration 009"
    );
}

/// Migration 013 widens the operation_type CHECK to include
/// `dismiss_suggestion` by rebuilding the table; this asserts the new value is
/// accepted after the rebuild (and so the constraint was recreated, not dropped).
#[tokio::test]
async fn mutations_operation_type_check_accepts_dismiss_suggestion() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    let ok = sqlx::query(
        "INSERT INTO mutations
            (id, client_mutation_id, device_id, user_id, list_id,
             entity_type, entity_id, operation_type, payload,
             base_revision, created_at_client, received_at_server)
         VALUES ('m1', 'cmid-dismiss', 'dev-1', 'user-1', 'list-1',
                 'canonical_item', 'e-1', 'dismiss_suggestion', '{}', 0, 't', 't')",
    )
    .execute(&pool)
    .await;
    assert!(
        ok.is_ok(),
        "dismiss_suggestion must be accepted after migration 013"
    );
}

/// Migration 012 widens the `match_feedback.action` CHECK to include `undo`
/// (issue #108) by rebuilding the table; this asserts the new value is accepted
/// after the rebuild (and so the constraint was recreated, not dropped) while the
/// foreign keys to `product_matches` / `users` still hold.
#[tokio::test]
async fn match_feedback_action_check_accepts_undo() {
    let pool = migrated_pool().await;
    seed_list_prereqs(&pool).await;

    sqlx::query(
        "INSERT INTO retailer_products (id, source, retailer, name, first_seen_at, last_seen_at) \
         VALUES ('rp-1', 'manual', 'tesco', 'Milk', 't', 't')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO product_matches \
             (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, \
              created_at, updated_at) \
         VALUES ('pm-1', ?, 'ci-1', 'rp-1', 'accepted', 'user', 't', 't')",
    )
    .bind(HOUSEHOLD)
    .execute(&pool)
    .await
    .unwrap();

    let ok = sqlx::query(
        "INSERT INTO match_feedback (id, product_match_id, user_id, action, created_at) \
         VALUES ('mf-1', 'pm-1', 'user-1', 'undo', 't')",
    )
    .execute(&pool)
    .await;
    assert!(ok.is_ok(), "undo must be accepted after migration 012");
}

/// Migration 010 seeds the shared sub-category catalogue and backfills
/// `canonical_items.subcategory_id` from the name-keyed seed mapping (issue #101).
///
/// The contract: every seeded item in a split category (Pantry, Drinks,
/// Household) is mapped to a sub-category that belongs to its OWN parent
/// category, so shopping mode can split a large section; items in unsplit
/// categories keep `subcategory_id = NULL` and fall back to the parent category.
#[tokio::test]
async fn migration_010_seeds_subcategories_and_backfills_items() {
    let pool = migrated_pool().await;

    const PANTRY: &str = "c1000000-0000-0000-0000-000000000005";
    const DRINKS: &str = "c1000000-0000-0000-0000-000000000008";
    const HOUSEHOLD: &str = "c1000000-0000-0000-0000-000000000010";
    const PRODUCE: &str = "c1000000-0000-0000-0000-000000000001";

    // 14 shared sub-categories: 6 Pantry + 4 Drinks + 4 Household.
    let subcategories: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM subcategories")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(subcategories, 14, "6 Pantry + 4 Drinks + 4 Household");

    // Every sub-category hangs off a real category and has a positive order.
    let orphans: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM subcategories s \
         LEFT JOIN categories c ON c.id = s.category_id \
         WHERE c.id IS NULL OR s.default_order < 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0, "sub-categories must reference a real parent");

    // A broad category now splits into several smaller groups: every Pantry item
    // the seed ships is mapped, and the 31 items span exactly 6 sub-categories.
    for (category, expected_subcategories, what) in [
        (PANTRY, 6, "Pantry"),
        (DRINKS, 4, "Drinks"),
        (HOUSEHOLD, 4, "Household"),
    ] {
        let unmapped: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM canonical_items \
             WHERE category_id = ?1 AND subcategory_id IS NULL",
        )
        .bind(category)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unmapped, 0, "every seeded {what} item must be mapped");

        let distinct: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT subcategory_id) FROM canonical_items WHERE category_id = ?1",
        )
        .bind(category)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            distinct, expected_subcategories,
            "{what} must split into {expected_subcategories} sub-categories"
        );
    }

    // A mapped sub-category always belongs to the item's own parent category.
    let cross_category: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM canonical_items ci \
         JOIN subcategories s ON s.id = ci.subcategory_id \
         WHERE s.category_id != ci.category_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cross_category, 0, "backfill must not cross categories");

    // Unsplit categories keep NULL, so their items fall back to the parent
    // category instead of disappearing.
    let produce_mapped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM canonical_items WHERE category_id = ?1 AND subcategory_id IS NOT NULL",
    )
    .bind(PRODUCE)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(produce_mapped, 0, "Produce is not split and must stay NULL");
}

/// Migration 013 creates `household_store_category_rules` with no back-filled
/// rows, so a household that configures nothing allows every category at every
/// store — the pre-#99 planning behaviour (issue #99).
#[tokio::test]
async fn migration_014_store_category_rules_start_empty() {
    let pool = migrated_pool().await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM household_store_category_rules")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no rules are seeded; the default is allow-all");
}

/// A whole-category rule and a sub-category refinement of the same category can
/// coexist, but a repeated scope is rejected by the unique index — the table
/// holds exactly one rule per (household, retailer, category, sub-category),
/// with NULL sub-category treated as its own scope (issue #99).
#[tokio::test]
async fn migration_014_allows_one_rule_per_scope() {
    const ALDI: &str = "aldi";
    const PANTRY: &str = "c1000000-0000-0000-0000-000000000005";
    const PASTA_RICE: &str = "d1000000-0000-0000-0000-000000000001";

    let pool = migrated_pool().await;
    let insert = |subcategory_id: Option<&str>| {
        let pool = pool.clone();
        let subcategory_id = subcategory_id.map(str::to_string);
        async move {
            sqlx::query(
                "INSERT INTO household_store_category_rules \
                     (household_id, retailer, category_id, subcategory_id, allowed, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 1, 't')",
            )
            .bind(HOUSEHOLD)
            .bind(ALDI)
            .bind(PANTRY)
            .bind(subcategory_id.as_deref())
            .execute(&pool)
            .await
        }
    };

    // Whole-category and sub-category rules coexist.
    insert(None).await.unwrap();
    insert(Some(PASTA_RICE)).await.unwrap();

    // Repeating either scope is rejected.
    assert!(insert(None).await.is_err(), "duplicate whole-category rule");
    assert!(
        insert(Some(PASTA_RICE)).await.is_err(),
        "duplicate sub-category rule"
    );
}
