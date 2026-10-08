//! Integration tests for the Tesco Clubcard importer (issue #107).
//!
//! Every fixture here is invented (made-up names and prices); no real export is
//! committed or read. The behaviours pinned are the acceptance criteria for the
//! slice: shape validation, idempotent re-import, unmatched lines preserved,
//! items without a name, online versus in-store, the `orders` section never
//! read, and imported purchases feeding the frequency model.

use std::future::Future;

use buywise_cli::commands::tesco;
use buywise_core::db;
use buywise_core::purchase_ledger;
use buywise_core::suggestions::{compute_suggestions, PurchaseDay};
use buywise_core::tesco_import::{self, TescoImportError};
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::{json, Value};
use sqlx::SqlitePool;

const HH_A: &str = "hh-a";
const HH_B: &str = "hh-b";
const T: &str = "2026-01-01T00:00:00Z";

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    sqlx::query(
        "INSERT INTO households (id, name, created_at) VALUES (?1, 'Home A', ?2), (?3, 'Home B', ?2)",
    )
    .bind(HH_A)
    .bind(T)
    .bind(HH_B)
    .execute(&pool)
    .await
    .expect("insert households");
    pool
}

async fn scalar(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("scalar")
}

/// Seed a canonical item plus an alias that the cascade resolves exactly.
async fn seed_item(pool: &SqlitePool, id: &str, household_id: &str, name_en: &str, alias: &str) {
    sqlx::query(
        "INSERT INTO canonical_items
            (id, household_id, name_en, name_es, category_id,
             default_constraint_policy, created_at)
         SELECT ?1, ?2, ?3, ?3, c.id, 'any', ?4
         FROM categories c LIMIT 1",
    )
    .bind(id)
    .bind(household_id)
    .bind(name_en)
    .bind(T)
    .execute(pool)
    .await
    .expect("seed canonical item");
    sqlx::query(
        "INSERT INTO item_aliases
            (id, household_id, canonical_item_id, alias, language, source, created_at)
         VALUES (?1, ?2, ?3, ?4, 'en', 'manual', ?5)",
    )
    .bind(format!("alias-{id}"))
    .bind(household_id)
    .bind(id)
    .bind(alias)
    .bind(T)
    .execute(pool)
    .await
    .expect("seed alias");
}

/// One invented item line. `name` is `None` for an item the export itemises
/// without a name.
fn item(name: Option<&str>, quantity: f64, weight: f64, price: f64, volume: f64) -> Value {
    json!({
        "name": name,
        "quantity": quantity,
        "weight": weight,
        "price": price,
        "volume": volume,
    })
}

/// One invented purchase.
fn purchase(
    timestamp: &str,
    store_type: &str,
    gross: f64,
    savings: f64,
    items: Vec<Value>,
) -> Value {
    json!({
        "timestamp": timestamp,
        "type": store_type,
        "says": "No",
        "basketValueGross": gross,
        "overallBasketSavings": savings,
        "basketValueNet": gross,
        "numberOfItems": items.len(),
        "payment": [{ "type": "card", "category": "VisaDebit", "amount": gross }],
        "items": items,
    })
}

fn export(purchases: Vec<Value>) -> Value {
    json!({ "requestId": "req-invented", "purchases": purchases })
}

async fn import(pool: &SqlitePool, document: &Value) -> tesco_import::TescoImportOutcome {
    tesco_import::import_tesco_export(
        pool,
        HH_A,
        document,
        Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap(),
    )
    .await
    .expect("import")
}

#[tokio::test]
async fn the_export_shape_is_validated() {
    // No `purchases` array at all.
    let err = tesco_import::parse_tesco_export(&json!({ "requestId": "x" }))
        .expect_err("missing purchases");
    let TescoImportError::Malformed(errors) = err else {
        panic!("expected Malformed");
    };
    assert!(errors
        .iter()
        .any(|e| e == "purchases: missing required field"));

    // A purchase missing its timestamp and an item missing its price, plus a
    // non-numeric gross.
    let document = json!({
        "purchases": [{
            "type": "IN_STORE",
            "basketValueGross": "sixteen",
            "basketValueNet": 16.0,
            "numberOfItems": 1,
            "items": [{ "name": "Thing", "quantity": 1, "weight": 0.0, "volume": 1.0 }]
        }]
    });
    let err = tesco_import::parse_tesco_export(&document).expect_err("malformed");
    let TescoImportError::Malformed(errors) = err else {
        panic!("expected Malformed");
    };
    assert!(errors
        .iter()
        .any(|e| e == "purchases[0].timestamp: missing required field"));
    assert!(errors
        .iter()
        .any(|e| e == "purchases[0].basketValueGross: expected an amount"));
    assert!(errors
        .iter()
        .any(|e| e == "purchases[0].items[0].price: missing required field"));
}

#[tokio::test]
async fn reimporting_the_same_export_is_idempotent() {
    let pool = setup_pool().await;
    seed_item(&pool, "item-milk", HH_A, "Milk", "milk").await;

    let document = export(vec![purchase(
        "2026-01-05 09:00:00.000",
        "IN_STORE",
        5.50,
        0.0,
        vec![
            item(Some("milk"), 2.0, 0.0, 1.25, 2.0),
            item(Some("Mystery Widget 4000"), 1.0, 0.0, 3.0, 1.0),
        ],
    )]);

    let first = import(&pool, &document).await;
    assert_eq!(
        (
            first.purchases,
            first.inserted,
            first.updated,
            first.unchanged
        ),
        (1, 1, 0, 0)
    );
    assert_eq!(
        (first.lines, first.matched_lines, first.unmatched_lines),
        (2, 1, 1)
    );
    assert_eq!(first.events_recorded, 1);

    let second = import(&pool, &document).await;
    assert_eq!(
        (
            second.purchases,
            second.inserted,
            second.updated,
            second.unchanged
        ),
        (1, 0, 0, 1)
    );
    assert_eq!(second.events_recorded, 0, "evidence must not be duplicated");

    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        1
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_lines").await,
        2
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM purchase_events WHERE source = 'import'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn unmatched_lines_are_preserved_verbatim() {
    let pool = setup_pool().await;
    let document = export(vec![purchase(
        "2026-01-05 09:00:00",
        "IN_STORE",
        3.0,
        0.0,
        vec![item(Some("Mystery Widget 4000 "), 1.0, 0.0, 3.0, 1.0)],
    )]);

    let outcome = import(&pool, &document).await;
    assert_eq!(outcome.unmatched_lines, 1);
    assert_eq!(outcome.matched_lines, 0);
    assert_eq!(outcome.events_recorded, 0);

    let transaction_id: String = sqlx::query_scalar("SELECT id FROM purchase_transactions LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let detail = purchase_ledger::get_transaction(&pool, HH_A, &transaction_id)
        .await
        .unwrap()
        .expect("transaction");
    assert_eq!(detail.lines.len(), 1);
    // The ledger trims the line text; the untrimmed original survives verbatim
    // in the preserved source payload.
    assert_eq!(detail.lines[0].original_text, "Mystery Widget 4000");
    assert_eq!(
        detail.lines[0].match_status,
        purchase_ledger::MATCH_UNMATCHED
    );
    assert_eq!(detail.lines[0].canonical_item_id, None);
    let payload: Value = serde_json::from_str(&detail.source_payload).unwrap();
    assert_eq!(payload["items"][0]["name"], "Mystery Widget 4000 ");
    assert_eq!(payload["items"][0]["volume"], 1.0);
    assert_eq!(payload["items"][0]["price"], 3.0);
}

#[tokio::test]
async fn items_without_a_name_are_kept() {
    let pool = setup_pool().await;
    let document = export(vec![purchase(
        "2026-01-05 09:00:00",
        "IN_STORE",
        2.50,
        0.0,
        vec![item(None, 1.0, 0.0, 2.5, 1.0)],
    )]);

    let outcome = import(&pool, &document).await;
    assert_eq!(
        (
            outcome.lines,
            outcome.matched_lines,
            outcome.unmatched_lines
        ),
        (1, 0, 1)
    );

    let transaction_id: String = sqlx::query_scalar("SELECT id FROM purchase_transactions LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let detail = purchase_ledger::get_transaction(&pool, HH_A, &transaction_id)
        .await
        .unwrap()
        .expect("transaction");
    assert_eq!(
        detail.lines[0].original_text,
        tesco_import::UNNAMED_LINE_TEXT
    );
    assert_eq!(detail.lines[0].name, None);
    assert_eq!(
        detail.lines[0].match_status,
        purchase_ledger::MATCH_UNMATCHED
    );
}

#[tokio::test]
async fn online_and_in_store_purchases_are_both_imported() {
    let pool = setup_pool().await;
    let document = export(vec![
        purchase(
            "2026-01-05 09:00:00.000",
            "IN_STORE",
            10.0,
            1.5,
            vec![item(Some("in-store thing"), 1.0, 0.0, 10.0, 1.0)],
        ),
        purchase(
            "2026-01-06 18:30:00.000",
            "GHS",
            20.0,
            0.0,
            vec![item(Some("online thing"), 2.0, 0.0, 10.0, 2.0)],
        ),
    ]);

    let outcome = import(&pool, &document).await;
    assert_eq!((outcome.purchases, outcome.inserted), (2, 2));

    let payloads: Vec<String> = sqlx::query_scalar(
        "SELECT source_payload FROM purchase_transactions ORDER BY purchase_date",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let first: Value = serde_json::from_str(&payloads[0]).unwrap();
    let second: Value = serde_json::from_str(&payloads[1]).unwrap();
    assert_eq!(first["type"], "IN_STORE");
    assert_eq!(second["type"], "GHS");
    // Totals and savings survive in the original observation.
    assert_eq!(first["basketValueGross"], 10.0);
    assert_eq!(first["overallBasketSavings"], 1.5);
    assert_eq!(second["numberOfItems"], 1);
}

#[tokio::test]
async fn refund_lines_keep_their_signed_amounts() {
    let pool = setup_pool().await;
    let document = export(vec![purchase(
        "2026-03-27 14:22:30.028",
        "GHS",
        -2.49,
        0.0,
        vec![item(Some("Strawberry Refund"), -1.0, 0.0, 2.49, -1.0)],
    )]);

    let outcome = import(&pool, &document).await;
    assert_eq!((outcome.purchases, outcome.inserted), (1, 1));

    let (total, line_total, quantity): (i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT pt.total_cents, pl.line_total_cents, pl.quantity \
         FROM purchase_transactions pt JOIN purchase_lines pl ON pl.transaction_id = pt.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, -249);
    assert_eq!(line_total, -249);
    assert_eq!(
        quantity, None,
        "a non-positive count is not stored as quantity"
    );

    let payload: String = sqlx::query_scalar("SELECT source_payload FROM purchase_transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["basketValueGross"], -2.49);
    assert_eq!(payload["items"][0]["volume"], -1.0);
}

#[tokio::test]
async fn refunds_do_not_create_frequency_evidence() {
    let pool = setup_pool().await;
    seed_item(&pool, "item-milk", HH_A, "Milk", "milk").await;
    let document = export(vec![
        purchase(
            "2026-03-27 14:22:30.028",
            "GHS",
            2.49,
            0.0,
            vec![
                item(Some("milk"), 1.0, 0.0, 2.49, 1.0),
                item(Some("milk"), -1.0, 0.0, 2.49, -1.0),
            ],
        ),
        purchase(
            "2026-03-28 14:22:30.028",
            "GHS",
            -2.49,
            0.0,
            vec![item(Some("milk"), 1.0, 0.0, 2.49, 1.0)],
        ),
    ]);

    let outcome = import(&pool, &document).await;
    assert_eq!(outcome.matched_lines, 3);
    assert_eq!(outcome.events_recorded, 1);

    let events = scalar(
        &pool,
        "SELECT COUNT(*) FROM purchase_events WHERE source = 'import'",
    )
    .await;
    assert_eq!(events, 1, "only the ordinary positive line is evidence");
    let (refund_line_total, refund_quantity): (i64, Option<f64>) = sqlx::query_as(
        "SELECT line_total_cents, quantity FROM purchase_lines \
         WHERE line_total_cents < 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(refund_line_total, -249);
    assert_eq!(refund_quantity, None);
}

#[tokio::test]
async fn the_orders_section_is_never_read() {
    let pool = setup_pool().await;
    let mut document = export(vec![purchase(
        "2026-01-05 09:00:00",
        "IN_STORE",
        4.0,
        0.0,
        vec![item(Some("Thing"), 1.0, 0.0, 4.0, 1.0)],
    )]);
    document["orders"] = json!([{
        "deliveryAddress": "SENTINEL-ADDRESS-123",
        "phone": "SENTINEL-PHONE-456",
    }]);

    let outcome = import(&pool, &document).await;
    assert_eq!((outcome.purchases, outcome.inserted), (1, 1));

    // No stored payload (or any other column) carries anything from `orders`.
    let payloads: Vec<String> =
        sqlx::query_scalar("SELECT source_payload FROM purchase_transactions")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(payloads.iter().all(|payload| !payload.contains("SENTINEL")));

    // A document with no `orders` at all imports just as well.
    let no_orders = export(vec![purchase(
        "2026-01-06 09:00:00",
        "IN_STORE",
        4.0,
        0.0,
        vec![item(Some("Thing"), 1.0, 0.0, 4.0, 1.0)],
    )]);
    let outcome = import(&pool, &no_orders).await;
    assert_eq!(outcome.inserted, 1);
}

#[tokio::test]
async fn matched_lines_feed_the_frequency_model() {
    let pool = setup_pool().await;
    seed_item(&pool, "item-milk", HH_A, "Milk", "milk").await;

    let document = export(vec![
        purchase(
            "2026-01-01 09:00:00",
            "IN_STORE",
            1.25,
            0.0,
            vec![item(Some("milk"), 1.0, 0.0, 1.25, 1.0)],
        ),
        purchase(
            "2026-01-08 09:00:00",
            "GHS",
            1.25,
            0.0,
            vec![item(Some("milk"), 1.0, 0.0, 1.25, 1.0)],
        ),
        purchase(
            "2026-01-15 09:00:00",
            "IN_STORE",
            1.25,
            0.0,
            vec![item(Some("milk"), 1.0, 0.0, 1.25, 1.0)],
        ),
    ]);

    let outcome = import(&pool, &document).await;
    assert_eq!((outcome.matched_lines, outcome.events_recorded), (3, 3));

    // The evidence the suggestion endpoint reads: three `import` events for the
    // canonical item, on the purchases' own days.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT canonical_item_id, occurred_at FROM purchase_events \
         WHERE household_id = ?1 AND source = 'import' ORDER BY occurred_at",
    )
    .bind(HH_A)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|(item, _)| item == "item-milk"));

    let evidence: Vec<PurchaseDay> = rows
        .iter()
        .map(|(item, at)| PurchaseDay {
            canonical_item_id: item.clone(),
            day: NaiveDate::parse_from_str(&at[..10], "%Y-%m-%d").unwrap(),
        })
        .collect();
    let suggestions = compute_suggestions(
        &evidence,
        &[],
        &std::collections::HashSet::new(),
        NaiveDate::from_ymd_opt(2026, 2, 1).unwrap(),
    );
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].canonical_item_id, "item-milk");
    assert_eq!(suggestions[0].typical_interval_days, 7);
}

/// The CLI wrapper writes into a real database file and shares the core rules:
/// importing the same export twice leaves one purchase and one evidence row.
#[test]
fn cli_import_tesco_writes_into_a_database_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("buywise.db");
    let json_path = dir.path().join("tesco.json");
    let db_url = format!("sqlite://{}", db_path.display());

    let pool = block_on(async {
        let pool = db::connect(&db_url).await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Home', ?2)")
            .bind(HH_A)
            .bind(T)
            .execute(&pool)
            .await
            .expect("insert household");
        seed_item(&pool, "item-milk", HH_A, "Milk", "milk").await;
        pool
    });

    let document = export(vec![purchase(
        "2026-01-05 09:00:00",
        "IN_STORE",
        1.25,
        0.0,
        vec![item(Some("milk"), 1.0, 0.0, 1.25, 1.0)],
    )]);
    std::fs::write(&json_path, document.to_string()).expect("write json");

    tesco::import_run(db_path.to_str().unwrap(), HH_A, json_path.to_str().unwrap())
        .expect("cli import");
    tesco::import_run(db_path.to_str().unwrap(), HH_A, json_path.to_str().unwrap())
        .expect("cli re-import");

    let (transactions, events) = block_on(async {
        (
            scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
            scalar(
                &pool,
                "SELECT COUNT(*) FROM purchase_events WHERE source = 'import'",
            )
            .await,
        )
    });
    assert_eq!(transactions, 1, "re-running the CLI must not duplicate");
    assert_eq!(events, 1);
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}
