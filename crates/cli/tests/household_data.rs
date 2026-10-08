//! Integration tests for the household data-control CLI commands (issue #119).
//!
//! Two seeded households pin the two acceptance criteria:
//!
//! * `export_household` writes a JSON document containing the household's
//!   catalogue, preferences, matches, lists with their entries and purchase
//!   events, and the test re-reads it with `serde_json::Value` — never the
//!   writer's own types — so the document is proven to stand alone.
//! * `delete_household` removes every household-scoped row for the target
//!   household in one transaction while every row of a second household is
//!   byte-for-byte unchanged; the per-table counts are computed here with
//!   independent queries, not by reusing the delete statements.

use std::collections::BTreeMap;
use std::future::Future;

use buywise_cli::commands::household::{self, HOUSEHOLD_TABLES};
use buywise_core::db;
use serde_json::Value;
use sqlx::SqlitePool;

const HH_A: &str = "hh-a";
const HH_B: &str = "hh-b";

/// A fixed timestamp so seeded rows are reproducible (the export echoes stored
/// timestamps; only `exported_at` is wall-clock).
const T: &str = "2026-01-01T00:00:00Z";

/// Every household-scoped table the delete must clear, with an independent
/// count query (`?1` = household id) that the delete code does not share.
///
/// `sessions` and `users` are absent on purpose: a user is not owned by one
/// household (a shared member must survive), so they are asserted separately.
const HOUSEHOLD_ROW_COUNTS: &[(&str, &str)] = &[
    (
        "list_entry_events",
        "SELECT COUNT(*) FROM list_entry_events e \
         JOIN list_entries le ON le.id = e.entry_id \
         JOIN shopping_lists sl ON sl.id = le.list_id WHERE sl.household_id = ?1",
    ),
    (
        "savings_events",
        "SELECT COUNT(*) FROM savings_events s \
         JOIN shopping_lists sl ON sl.id = s.list_id WHERE sl.household_id = ?1",
    ),
    (
        "purchase_events",
        "SELECT COUNT(*) FROM purchase_events WHERE household_id = ?1",
    ),
    (
        "purchase_transaction_revisions",
        "SELECT COUNT(*) FROM purchase_transaction_revisions pr \
         JOIN purchase_transactions pt ON pt.id = pr.transaction_id \
         WHERE pt.household_id = ?1",
    ),
    (
        "purchase_lines",
        "SELECT COUNT(*) FROM purchase_lines pl \
         JOIN purchase_transactions pt ON pt.id = pl.transaction_id \
         WHERE pt.household_id = ?1",
    ),
    (
        "purchase_transactions",
        "SELECT COUNT(*) FROM purchase_transactions WHERE household_id = ?1",
    ),
    (
        "mutations",
        "SELECT COUNT(*) FROM mutations WHERE list_id IN \
         (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    ),
    (
        "list_entries",
        "SELECT COUNT(*) FROM list_entries WHERE list_id IN \
         (SELECT id FROM shopping_lists WHERE household_id = ?1)",
    ),
    (
        "recommendation_runs",
        "SELECT COUNT(*) FROM recommendation_runs WHERE household_id = ?1",
    ),
    (
        "shopping_lists",
        "SELECT COUNT(*) FROM shopping_lists WHERE household_id = ?1",
    ),
    (
        "match_feedback",
        "SELECT COUNT(*) FROM match_feedback WHERE product_match_id IN \
         (SELECT id FROM product_matches WHERE household_id = ?1)",
    ),
    (
        "product_matches",
        "SELECT COUNT(*) FROM product_matches WHERE household_id = ?1",
    ),
    (
        "item_constraints",
        "SELECT COUNT(*) FROM item_constraints WHERE canonical_item_id IN \
         (SELECT id FROM canonical_items WHERE household_id = ?1)",
    ),
    (
        "item_aliases",
        "SELECT COUNT(*) FROM item_aliases WHERE household_id = ?1",
    ),
    (
        "purchase_dismissals",
        "SELECT COUNT(*) FROM purchase_dismissals WHERE household_id = ?1",
    ),
    (
        "line_name_embeddings",
        "SELECT COUNT(*) FROM line_name_embeddings WHERE household_id = ?1",
    ),
    (
        "catalogue_embeddings",
        "SELECT COUNT(*) FROM catalogue_embeddings WHERE household_id = ?1",
    ),
    (
        "canonical_items",
        "SELECT COUNT(*) FROM canonical_items WHERE household_id = ?1",
    ),
    (
        "household_store_category_rules",
        "SELECT COUNT(*) FROM household_store_category_rules WHERE household_id = ?1",
    ),
    (
        "household_store_settings",
        "SELECT COUNT(*) FROM household_store_settings WHERE household_id = ?1",
    ),
    (
        "household_settings",
        "SELECT COUNT(*) FROM household_settings WHERE household_id = ?1",
    ),
    (
        "store_category_orders",
        "SELECT COUNT(*) FROM store_category_orders WHERE household_id = ?1",
    ),
    (
        "receipt_images",
        "SELECT COUNT(*) FROM receipt_images WHERE household_id = ?1",
    ),
    (
        "household_members",
        "SELECT COUNT(*) FROM household_members WHERE household_id = ?1",
    ),
    (
        "households",
        "SELECT COUNT(*) FROM households WHERE id = ?1",
    ),
];

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

async fn exec(pool: &SqlitePool, sql: &str) {
    sqlx::query(sql)
        .execute(pool)
        .await
        .expect("seed statement");
}

/// One receipt image each for household A and household B, with a real file on
/// disk under `dir`, so a household delete has bytes to remove as well as rows.
///
/// The keys are fixed (32 hex characters plus the extension) so the test can
/// name the files it expects to survive or disappear; upload mints random ones.
async fn seed_receipt_rows(pool: &SqlitePool, dir: &std::path::Path) {
    const KEY_A: &str = "aaaabbbbccccddddeeeeffff00001111.png";
    const KEY_B: &str = "11112222333344445555666677778888.jpg";
    exec(
        pool,
        &format!(
            "INSERT INTO receipt_images \
             (id, household_id, uploaded_by, uploaded_at, expires_at, content_type, \
              byte_size, sha256_hex, storage_key, processing) VALUES \
             ('ri-a', '{HH_A}', 'u-a', '{T}', '2030-02-01T00:00:00Z', 'image/png', \
              4, 'aa', '{KEY_A}', 'local'), \
             ('ri-b', '{HH_B}', 'u-b', '{T}', '2030-02-01T00:00:00Z', 'image/jpeg', \
              4, 'bb', '{KEY_B}', 'local')"
        ),
    )
    .await;
    for key in [KEY_A, KEY_B] {
        buywise_core::receipts::write_file(dir, key, b"img!").expect("write receipt file");
    }
}

/// Seed two households that exercise every table the commands touch.
///
/// Household A owns one of everything; household B owns the same shapes with
/// different ids so a leak in either direction is visible. `u-shared` belongs to
/// both (must survive A's deletion); `u-a` belongs only to A (must not).
async fn seed_fixture(pool: &SqlitePool) {
    exec(
        pool,
        &format!(
            "INSERT INTO households (id, name, created_at) VALUES \
         ('{HH_A}', 'Home A', '{T}'), ('{HH_B}', 'Home B', '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO users (id, email, password_hash, display_name, created_at) VALUES \
         ('u-a', 'a@example.test', 'hash', 'Ann', '{T}'), \
         ('u-shared', 's@example.test', 'hash', 'Sam', '{T}'), \
         ('u-b', 'b@example.test', 'hash', 'Bob', '{T}'), \
         ('u-unrelated', 'unrelated@example.test', 'hash', 'Unaffiliated', '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO household_members (household_id, user_id, role, joined_at) VALUES \
         ('{HH_A}', 'u-a', 'owner', '{T}'), \
         ('{HH_A}', 'u-shared', 'member', '{T}'), \
         ('{HH_B}', 'u-shared', 'owner', '{T}'), \
         ('{HH_B}', 'u-b', 'member', '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
        "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at) VALUES \
         ('sess-a', 'u-a', 'th-a', 'csrf-a', '{T}', '2030-01-01T00:00:00Z'), \
         ('sess-shared', 'u-shared', 'th-s', 'csrf-s', '{T}', '2030-01-01T00:00:00Z'), \
         ('sess-b', 'u-b', 'th-b', 'csrf-b', '{T}', '2030-01-01T00:00:00Z'), \
         ('sess-unrelated', 'u-unrelated', 'th-u', 'csrf-u', '{T}', '2030-01-01T00:00:00Z')"
    ),
    )
    .await;
    exec(
        pool,
        "INSERT INTO categories (id, name_en, name_es, icon, default_order, is_fresh) \
         VALUES ('cat-test', 'Test', 'Prueba', 'x', 1, 0)",
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO canonical_items \
             (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES ('ci-a', '{HH_A}', 'Apples', 'Manzanas', 'cat-test', 'any', '{T}'), \
                ('ci-b', '{HH_B}', 'Bread', 'Pan', 'cat-test', 'any', '{T}')"
    ))
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO item_aliases \
             (id, household_id, canonical_item_id, alias, language, source, created_at) \
         VALUES ('al-a', '{HH_A}', 'ci-a', 'apples', 'en', 'manual', '{T}')"
        ),
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO item_constraints (id, canonical_item_id, brand, substitution_policy, updated_at) \
         VALUES ('ic-a', 'ci-a', 'BrandA', 'any', '{T}')"
    ))
    .await;
    // Derived embedding vectors for A only (B must stay untouched by A's delete).
    exec(
        pool,
        &format!(
            "INSERT INTO catalogue_embeddings \
             (id, household_id, canonical_item_id, text, model, dim, vector, created_at) \
         VALUES ('ce-a', '{HH_A}', 'ci-a', 'apples', 'test-model', 2, X'0000803F00000000', '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
        "INSERT INTO line_name_embeddings (id, household_id, name, model, dim, vector, created_at) \
         VALUES ('lne-a', '{HH_A}', 'Apples 1kg', 'test-model', 2, X'0000803F00000000', '{T}')"
    ),
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO household_settings \
             (household_id, convenience_threshold_2_cents, convenience_threshold_3_cents, updated_at) \
         VALUES ('{HH_A}', 1200, 3000, '{T}'), ('{HH_B}', 1000, 2500, '{T}')"
    ))
    .await;
    exec(
        pool,
        &format!(
        "INSERT INTO household_store_settings (household_id, retailer, status, nearby, updated_at) \
         VALUES ('{HH_A}', 'tesco', 'off', 1, '{T}'), ('{HH_B}', 'tesco', 'active', 0, '{T}')"
    ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO household_store_category_rules \
             (household_id, retailer, category_id, subcategory_id, allowed, updated_at) \
         VALUES ('{HH_A}', 'aldi', 'cat-test', NULL, 0, '{T}')"
        ),
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO store_category_orders (id, household_id, store, category_id, display_order, updated_at) \
         VALUES ('sco-a', '{HH_A}', 'tesco', 'cat-test', 7, '{T}')"
    ))
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO retailer_products \
             (id, source, retailer, external_id, name, pack_count, first_seen_at, last_seen_at) \
         VALUES ('rp-1', 'basketwatch', 'tesco', 'ext-1', 'Apples 1kg', 1, '{T}', '{T}')"
        ),
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO product_matches \
             (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, created_at, updated_at) \
         VALUES ('pm-a', '{HH_A}', 'ci-a', 'rp-1', 'rejected', 'user', '{T}', '{T}'), \
                ('pm-b', '{HH_B}', 'ci-b', 'rp-1', 'accepted', 'user', '{T}', '{T}')"
    ))
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO match_feedback (id, product_match_id, user_id, action, created_at) \
         VALUES ('mf-a', 'pm-a', 'u-a', 'reject', '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
        "INSERT INTO shopping_lists (id, household_id, name, status, current_revision, created_at) \
         VALUES ('list-a', '{HH_A}', 'Weekly', 'active', 3, '{T}'), \
                ('list-b', '{HH_B}', 'Weekly B', 'active', 1, '{T}')"
    ),
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO list_entries \
             (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, added_by, \
              quantity, status, effective_constraint_hash, last_updated_revision, created_at, updated_at) \
         VALUES ('entry-a', 'list-a', 'ci-a', 'resolved', 'apples', 'Apples', 'u-a', 2, 'active', \
                 'default', 3, '{T}', '{T}'), \
                ('entry-b', 'list-b', 'ci-b', 'resolved', 'bread', 'Bread', 'u-b', 1, 'active', \
                 'default', 1, '{T}', '{T}')"
    ))
    .await;
    exec(
        pool,
        &format!(
        "INSERT INTO list_entry_events (id, entry_id, user_id, event_type, payload, created_at) \
         VALUES ('ee-a', 'entry-a', 'u-a', 'added', '{{}}', '{T}')"
    ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO mutations \
             (id, client_mutation_id, device_id, user_id, list_id, entity_type, entity_id, \
              operation_type, payload, base_revision, created_at_client, received_at_server, \
              server_revision, status, response_json) \
         VALUES ('mut-a', 'cm-a', 'dev-a', 'u-a', 'list-a', 'list_entry', 'entry-a', 'add_item', \
                 '{{}}', 2, '{T}', '{T}', 3, 'accepted', '{{}}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO recommendation_runs \
             (id, list_id, household_id, run_at, plans_json, recommended_plan, \
              convenience_threshold_2, convenience_threshold_3) \
         VALUES ('run-a', 'list-a', '{HH_A}', '{T}', '[]', 'best_single', 1200, 3000)"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO savings_events \
             (id, list_id, recommendation_run_id, baseline_cost_cents, recommended_cost_cents, \
              theoretical_minimum_cost_cents, gross_saving_cents, created_at) \
         VALUES ('sav-a', 'list-a', 'run-a', 1000, 900, 850, 100, '{T}')"
        ),
    )
    .await;
    exec(pool, &format!(
        "INSERT INTO purchase_events \
             (id, household_id, canonical_item_id, list_id, entry_id, source, occurred_at, recorded_at) \
         VALUES ('pe-a', '{HH_A}', 'ci-a', 'list-a', 'entry-a', 'check_off', '{T}', '{T}')"
    ))
    .await;
    exec(pool, &format!(
        "INSERT INTO purchase_dismissals (id, household_id, canonical_item_id, dismissed_by, dismissed_at) \
         VALUES ('pd-a', '{HH_A}', 'ci-a', 'u-a', '{T}')"
    ))
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO purchase_transactions \
             (id, household_id, retailer, purchase_date, source_kind, source_transaction_id, \
              transaction_identity, identity_kind, fingerprint, currency, total_cents, revision, \
              source_payload, created_at, updated_at) \
         VALUES ('pt-a', '{HH_A}', 'tesco', '2026-05-01', 'import', 'SRC-A', 'SRC-A', 'source_id', \
                 'pl1:aaaa', 'EUR', 500, 2, '{{\"source\":\"a\"}}', '{T}', '{T}'), \
                ('pt-b', '{HH_B}', 'lidl', '2026-05-02', 'import', NULL, 'pl1:bbbb', 'fingerprint', \
                 'pl1:bbbb', 'EUR', 700, 1, '{{}}', '{T}', '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO purchase_lines \
             (id, transaction_id, line_ordinal, original_text, name, quantity, line_total_cents, \
              retailer_product_id, canonical_item_id, match_status, revision, created_at) \
         VALUES ('pl-a', 'pt-a', 0, 'Apples 1kg', 'Apples', 1, 500, 'rp-1', 'ci-a', 'matched', 2, '{T}'), \
                ('pl-b', 'pt-b', 0, 'Bread', NULL, 1, 700, NULL, NULL, 'unmatched', 1, '{T}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO purchase_transaction_revisions \
             (id, transaction_id, revision, retailer, purchase_date, currency, total_cents, \
              source_payload, lines_json, superseded_at) \
         VALUES ('ptr-a', 'pt-a', 1, 'tesco', '2026-05-01', 'EUR', 500, '{{}}', '[]', '{T}')"
        ),
    )
    .await;
}

/// Per-table household row counts, computed with the test's own queries.
async fn counts(pool: &SqlitePool, household_id: &str) -> BTreeMap<&'static str, i64> {
    let mut out = BTreeMap::new();
    for (table, sql) in HOUSEHOLD_ROW_COUNTS {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(household_id)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("count {table}: {e}"));
        out.insert(*table, n);
    }
    out
}

async fn scalar(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("scalar")
}

/// Export through a JSON round trip, so assertions run against the serialised
/// document a consumer would actually read.
async fn export_document(pool: &SqlitePool, household_id: &str) -> Value {
    let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let document = household::export_household(pool, household_id, now)
        .await
        .expect("export household");
    let text = serde_json::to_string(&document).expect("serialise export");
    serde_json::from_str(&text).expect("re-parse export without app types")
}

fn array_len(value: &Value, pointer: &str) -> usize {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{pointer} is not an array"))
        .len()
}

#[tokio::test]
async fn export_contains_the_documented_household_records_with_intact_links() {
    let pool = setup_pool().await;
    seed_fixture(&pool).await;
    let receipts_dir = tempfile::tempdir().expect("tempdir");
    seed_receipt_rows(&pool, receipts_dir.path()).await;
    let doc = export_document(&pool, HH_A).await;

    assert_eq!(doc["format"], household::EXPORT_FORMAT);
    assert_eq!(doc["format_version"], household::EXPORT_FORMAT_VERSION);
    assert_eq!(doc["household"]["id"], HH_A);
    assert_eq!(doc["household"]["name"], "Home A");

    // Members resolve the user ids the rest of the document links to, without
    // exporting credentials.
    assert_eq!(array_len(&doc, "/members"), 2);
    assert!(doc.to_string().contains("Ann"));
    assert!(
        !doc.to_string().contains("password"),
        "the export must not carry password hashes"
    );

    // Catalogue: item plus its aliases and constraint, all household-scoped.
    assert_eq!(array_len(&doc, "/catalogue"), 1);
    let item = &doc["catalogue"][0];
    assert_eq!(item["id"], "ci-a");
    assert_eq!(array_len(item, "/aliases"), 1);
    assert_eq!(item["aliases"][0]["alias"], "apples");
    assert_eq!(item["constraint"]["brand"], "BrandA");

    // Preferences: thresholds, store disposition, category rules, aisle order,
    // dismissals.
    assert_eq!(
        doc["preferences"]["settings"]["convenience_threshold_2_cents"],
        1200
    );
    assert_eq!(doc["preferences"]["stores"][0]["retailer"], "tesco");
    assert_eq!(doc["preferences"]["stores"][0]["status"], "off");
    assert_eq!(doc["preferences"]["stores"][0]["nearby"], 1);
    assert_eq!(doc["preferences"]["store_category_rules"][0]["allowed"], 0);
    assert_eq!(array_len(&doc, "/preferences/store_category_orders"), 1);
    assert_eq!(array_len(&doc, "/preferences/purchase_dismissals"), 1);

    // Rejected matches (and the retailer products they point at) are present.
    assert_eq!(array_len(&doc, "/product_matches"), 1);
    assert_eq!(doc["product_matches"][0]["status"], "rejected");
    assert_eq!(array_len(&doc, "/retailer_products"), 1);
    assert_eq!(doc["retailer_products"][0]["id"], "rp-1");
    assert_eq!(array_len(&doc, "/match_feedback"), 1);

    // Lists carry their entries and history.
    assert_eq!(array_len(&doc, "/lists"), 1);
    let list = &doc["lists"][0];
    assert_eq!(list["id"], "list-a");
    assert_eq!(array_len(list, "/entries"), 1);
    assert_eq!(list["entries"][0]["id"], "entry-a");
    assert_eq!(list["entries"][0]["quantity"], 2);
    assert_eq!(array_len(list, "/entry_events"), 1);
    assert_eq!(array_len(list, "/mutations"), 1);
    assert_eq!(array_len(list, "/recommendation_runs"), 1);
    assert_eq!(array_len(list, "/savings_events"), 1);

    // Purchase records.
    assert_eq!(array_len(&doc, "/purchase_events"), 1);
    assert_eq!(doc["purchase_events"][0]["id"], "pe-a");

    // Purchase ledger: transactions nest their lines and superseded revisions.
    assert_eq!(array_len(&doc, "/purchase_transactions"), 1);
    let tx = &doc["purchase_transactions"][0];
    assert_eq!(tx["id"], "pt-a");
    assert_eq!(tx["revision"], 2);
    assert_eq!(tx["source_transaction_id"], "SRC-A");
    assert_eq!(array_len(tx, "/lines"), 1);
    assert_eq!(tx["lines"][0]["original_text"], "Apples 1kg");
    assert_eq!(tx["lines"][0]["match_status"], "matched");
    assert_eq!(array_len(tx, "/revisions"), 1);
    assert_eq!(tx["revisions"][0]["revision"], 1);

    // Receipt-image metadata is exported; bytes and the internal file name are not.
    assert_eq!(array_len(&doc, "/receipt_images"), 1);
    let receipt = &doc["receipt_images"][0];
    assert_eq!(receipt["id"], "ri-a");
    assert_eq!(receipt["content_type"], "image/png");
    assert_eq!(receipt["processing"], "local");
    assert_eq!(receipt["uploaded_by"], "u-a");
    assert!(receipt["expires_at"].is_string());
    assert!(
        receipt.get("storage_key").is_none(),
        "the internal file name must not be exported"
    );

    // Reference data the links resolve against is embedded, not assumed.
    let category_ids: Vec<&str> = doc["reference_data"]["categories"]
        .as_array()
        .expect("categories")
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(category_ids.contains(&"cat-test"));

    // Every id a record points at is present in the document.
    let catalogue_ids: Vec<&str> = doc["catalogue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    for entry in doc["lists"][0]["entries"].as_array().unwrap() {
        assert!(catalogue_ids.contains(&entry["canonical_item_id"].as_str().unwrap()));
    }
    assert!(catalogue_ids.contains(
        &doc["purchase_events"][0]["canonical_item_id"]
            .as_str()
            .unwrap()
    ));
    let product_ids: Vec<&str> = doc["retailer_products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert!(product_ids.contains(
        &doc["product_matches"][0]["retailer_product_id"]
            .as_str()
            .unwrap()
    ));
    for row in doc["purchase_transactions"][0]["lines"].as_array().unwrap() {
        assert_eq!(
            row["transaction_id"], "pt-a",
            "line lost its transaction link"
        );
        if let Some(id) = row["canonical_item_id"].as_str() {
            assert!(catalogue_ids.contains(&id));
        }
        if let Some(id) = row["retailer_product_id"].as_str() {
            assert!(product_ids.contains(&id));
        }
    }
    for row in doc["purchase_transactions"][0]["revisions"]
        .as_array()
        .unwrap()
    {
        assert_eq!(row["transaction_id"], "pt-a");
    }
    for child in [
        "entries",
        "entry_events",
        "mutations",
        "recommendation_runs",
        "savings_events",
    ] {
        for row in doc["lists"][0][child].as_array().unwrap() {
            assert_eq!(row["list_id"], "list-a", "{child} row lost its list link");
        }
    }

    // Household B must not leak into A's export.
    assert!(!doc.to_string().contains("ci-b"));
    assert!(!doc.to_string().contains("list-b"));
    assert!(!doc.to_string().contains("pt-b"));
}

#[tokio::test]
async fn export_rejects_an_unknown_household() {
    let pool = setup_pool().await;
    seed_fixture(&pool).await;
    let err = household::export_household(
        &pool,
        "hh-missing",
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    )
    .await
    .expect_err("unknown household must fail");
    assert!(err.to_string().contains("no household"), "got {err}");
}

#[tokio::test]
async fn delete_removes_only_the_target_household() {
    let pool = setup_pool().await;
    seed_fixture(&pool).await;
    let receipts_dir = tempfile::tempdir().expect("tempdir");
    let receipts_dir = receipts_dir.path();
    seed_receipt_rows(&pool, receipts_dir).await;
    let key_a = receipts_dir.join("aaaabbbbccccddddeeeeffff00001111.png");
    let key_b = receipts_dir.join("11112222333344445555666677778888.jpg");
    assert!(key_a.exists() && key_b.exists());

    let before_a = counts(&pool, HH_A).await;
    let before_b = counts(&pool, HH_B).await;
    for (table, n) in &before_a {
        assert!(*n > 0, "fixture must seed at least one {table} row for A");
    }

    // Dry run executes the real statements and rolls back, and touches no file.
    let dry = household::delete_household(&pool, HH_A, true, receipts_dir)
        .await
        .expect("dry run");
    assert_eq!(
        counts(&pool, HH_A).await,
        before_a,
        "dry run must not delete"
    );
    assert_eq!(
        counts(&pool, HH_B).await,
        before_b,
        "dry run must not delete"
    );
    assert!(
        key_a.exists(),
        "a dry run must not remove receipt image files"
    );
    let reported: BTreeMap<&str, u64> = dry.iter().copied().collect();
    assert_eq!(reported["shopping_lists"], 1);
    assert_eq!(reported["households"], 1);
    assert_eq!(reported["receipt_images"], 1);
    assert_eq!(
        reported["users"], 1,
        "only the user left without a household"
    );

    // The delete covers every table this test verified.
    for (table, _) in HOUSEHOLD_ROW_COUNTS {
        assert!(
            HOUSEHOLD_TABLES.contains(table),
            "{table} is verified but not in HOUSEHOLD_TABLES"
        );
    }

    household::delete_household(&pool, HH_A, false, receipts_dir)
        .await
        .expect("delete");

    let after_a = counts(&pool, HH_A).await;
    for (table, n) in &after_a {
        assert_eq!(*n, 0, "{table} still holds rows for the deleted household");
    }
    assert_eq!(
        counts(&pool, HH_B).await,
        before_b,
        "household B must be untouched"
    );

    // Receipt images: A's file and row go with the household; B's stay.
    assert!(
        !key_a.exists(),
        "the deleted household's receipt image must be gone from disk"
    );
    assert!(
        key_b.exists(),
        "another household's receipt image must survive"
    );

    // Members: the orphan goes with their session; a shared member survives with
    // their own session and their membership in B intact.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM users WHERE id = 'u-a'").await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM users WHERE id = 'u-shared'").await,
        1
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM users WHERE id = 'u-b'").await,
        1
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM users WHERE id = 'u-unrelated'").await,
        1,
        "a membership-less user outside the target household must survive"
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM sessions WHERE id = 'sess-a'").await,
        0
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sessions WHERE id = 'sess-unrelated'"
        )
        .await,
        1,
        "the unrelated user's session must survive"
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sessions WHERE id = 'sess-shared'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM household_members WHERE user_id = 'u-shared' \
                       AND household_id = 'hh-b'"
        )
        .await,
        1
    );

    // Shared reference data is not household-identifiable and is retained.
    assert!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM retailer_products WHERE id = 'rp-1'"
        )
        .await
            == 1
    );
    assert!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM categories WHERE id = 'cat-test'"
        )
        .await
            == 1
    );
    assert!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM product_matches WHERE id = 'pm-b'"
        )
        .await
            == 1,
        "another household's match must survive"
    );

    assert!(
        household::export_household(
            &pool,
            HH_A,
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        )
        .await
        .is_err(),
        "the exported household is gone"
    );
}

#[tokio::test]
async fn delete_refuses_without_confirmation() {
    let err = household::delete_run("/nonexistent/buywise.db", HH_A, false, false, None)
        .expect_err("a bare delete must refuse");
    assert!(err.to_string().contains("--confirm"), "got {err}");
}

#[tokio::test]
async fn delete_rejects_an_unknown_household() {
    let pool = setup_pool().await;
    seed_fixture(&pool).await;
    let err = household::delete_household(&pool, "hh-missing", false, std::path::Path::new("/tmp"))
        .await
        .expect_err("unknown household must fail");
    assert!(err.to_string().contains("no household"), "got {err}");
}

/// Build a small runtime for the async DB-assertion helpers. `[test]` bodies
/// call the sync `run` wrappers (which build their own runtime) between
/// `block_on` calls, so a runtime is never nested.
fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(fut)
}

/// The `run` wrappers against a real database file: export writes the document
/// where asked, a dry run leaves the household in place, and a confirmed run
/// removes it.
#[test]
fn run_wrappers_write_the_export_file_and_gate_the_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("buywise.db");
    let db_path = db_path.to_str().expect("utf-8 path");

    let pool = block_on(async {
        let pool = db::connect(&format!("sqlite://{db_path}"))
            .await
            .expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        seed_fixture(&pool).await;
        pool
    });

    let out_path = dir.path().join("export.json");
    let out_path = out_path.to_str().expect("utf-8 path");
    household::export_run(db_path, HH_A, Some(out_path)).expect("export run");
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(out_path).expect("read export file"))
            .expect("parse export file");
    assert_eq!(written["household"]["id"], HH_A);
    assert_eq!(written["format"], household::EXPORT_FORMAT);

    household::delete_run(db_path, HH_A, false, true, None).expect("dry run");
    assert_eq!(
        block_on(scalar(
            &pool,
            "SELECT COUNT(*) FROM households WHERE id = 'hh-a'"
        )),
        1,
        "a dry run must leave the household in place"
    );

    household::delete_run(db_path, HH_A, true, false, None).expect("confirmed delete");
    assert_eq!(
        block_on(scalar(
            &pool,
            "SELECT COUNT(*) FROM households WHERE id = 'hh-a'"
        )),
        0
    );

    let err = household::export_run("/nonexistent/buywise.db", HH_A, None)
        .expect_err("a missing database must fail");
    assert!(err.to_string().contains("does not exist"), "got {err}");
}
