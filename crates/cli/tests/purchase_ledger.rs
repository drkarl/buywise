//! Integration tests for the purchase ledger core and CLI (issue #148).
//!
//! The behaviours pinned here are the acceptance criteria for the ledger slice:
//! a re-import is idempotent (one transaction, one set of lines), a correction
//! bumps the revision and keeps the earlier one, unmatched lines are preserved,
//! the ledger is household-scoped, deleting a transaction removes its lines and
//! history, and the worked example in `docs/purchase-ledger.md` parses.

use std::future::Future;

use buywise_cli::commands::purchases;
use buywise_core::db;
use buywise_core::purchase_ledger::{self, ImportError, ImportStatus};
use serde_json::{json, Value};
use sqlx::SqlitePool;

const HH_A: &str = "hh-a";
const HH_B: &str = "hh-b";
const T: &str = "2026-01-01T00:00:00Z";

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Home A', ?2), (?3, 'Home B', ?2)")
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

async fn seed_canonical_item(pool: &SqlitePool, id: &str, household_id: &str) {
    sqlx::query(
        "INSERT INTO canonical_items
            (id, household_id, name_en, name_es, category_id,
             default_constraint_policy, created_at)
         SELECT ?1, ?2, 'Milk', 'Leche', c.id, 'any', ?3
         FROM categories c LIMIT 1",
    )
    .bind(id)
    .bind(household_id)
    .bind(T)
    .execute(pool)
    .await
    .expect("seed canonical item");
}

/// A one-transaction document. Two lines: a described one whose total is
/// `line_total`, and a bare one that must stay `unmatched`.
fn document(source_transaction_id: Option<&str>, line_total: i64) -> Value {
    let mut transaction = json!({
        "retailer": "tesco",
        "purchase_date": "2026-05-01",
        "currency": "EUR",
        "total_cents": line_total + 100,
        "lines": [
            {
                "original_text": "Apples 1kg",
                "name": "Apples",
                "quantity": 1,
                "line_total_cents": line_total
            },
            { "original_text": "Mystery Item", "line_total_cents": 100 }
        ]
    });
    if let Some(id) = source_transaction_id {
        transaction["source_transaction_id"] = json!(id);
    }
    json!({ "format_version": 1, "transactions": [transaction] })
}

/// Import `document(source, line_total)` into household A.
async fn import(pool: &SqlitePool, document: &Value) -> purchase_ledger::ImportOutcome {
    purchase_ledger::import_transactions(pool, HH_A, document)
        .await
        .expect("import")
}

#[tokio::test]
async fn reimporting_a_transaction_is_idempotent() {
    let pool = setup_pool().await;
    let doc = document(Some("SRC-1"), 500);

    let first = import(&pool, &doc).await;
    assert_eq!(first.results[0].status, ImportStatus::Inserted);
    assert_eq!(first.results[0].revision, 1);
    assert_eq!((first.inserted, first.updated, first.unchanged), (1, 0, 0));

    let second = import(&pool, &doc).await;
    assert_eq!(second.results[0].status, ImportStatus::Unchanged);
    assert_eq!(
        second.results[0].transaction_id,
        first.results[0].transaction_id
    );
    assert_eq!(
        (second.inserted, second.updated, second.unchanged),
        (0, 0, 1)
    );

    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        1
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_lines").await,
        2
    );
    let total_eur: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(total_cents), 0) FROM purchase_transactions \
         WHERE household_id = ?1 AND currency = ?2",
    )
    .bind(HH_A)
    .bind("EUR")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total_eur, 600);
}

#[tokio::test]
async fn a_correction_bumps_the_revision_and_keeps_the_earlier_one() {
    let pool = setup_pool().await;
    let first = import(&pool, &document(Some("SRC-1"), 500)).await;
    let transaction_id = first.results[0].transaction_id.clone();

    let corrected = import(&pool, &document(Some("SRC-1"), 600)).await;
    assert_eq!(corrected.results[0].status, ImportStatus::Updated);
    assert_eq!(corrected.results[0].revision, 2);
    assert_eq!(corrected.results[0].transaction_id, transaction_id);

    let detail = purchase_ledger::get_transaction(&pool, HH_A, &transaction_id)
        .await
        .unwrap()
        .expect("transaction");
    assert_eq!(detail.transaction.revision, 2);
    assert_eq!(detail.transaction.total_cents, 700);
    assert_eq!(detail.lines[0].line_total_cents, 600);

    // The superseded revision is kept for audit, with its own total and lines.
    assert_eq!(detail.revisions.len(), 1);
    assert_eq!(detail.revisions[0].revision, 1);
    assert_eq!(detail.revisions[0].total_cents, 600);
    assert!(detail.revisions[0].lines.to_string().contains("Apples 1kg"));
}

#[tokio::test]
async fn source_transaction_ids_are_scoped_by_normalized_retailer() {
    let pool = setup_pool().await;
    let tesco = document(Some("SRC-SHARED"), 500);
    let mut tesco_mixed_case = tesco.clone();
    tesco_mixed_case["transactions"][0]["retailer"] = json!("  Tesco  ");
    let mut lidl = document(Some("SRC-SHARED"), 700);
    lidl["transactions"][0]["retailer"] = json!("LIDL");

    let tesco_result = import(&pool, &tesco).await;
    let lidl_result = import(&pool, &lidl).await;
    let same_tesco = import(&pool, &tesco_mixed_case).await;

    assert_eq!(tesco_result.results[0].status, ImportStatus::Inserted);
    assert_eq!(lidl_result.results[0].status, ImportStatus::Inserted);
    assert_ne!(
        tesco_result.results[0].transaction_id,
        lidl_result.results[0].transaction_id
    );
    assert_eq!(same_tesco.results[0].status, ImportStatus::Unchanged);
    assert_eq!(
        same_tesco.results[0].transaction_id,
        tesco_result.results[0].transaction_id
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        2
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM purchase_transaction_revisions WHERE revision = 1"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM purchase_transactions WHERE revision != 1"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn fingerprint_identity_is_stable_and_only_corrects_non_identity_fields() {
    let pool = setup_pool().await;
    let doc = document(None, 500);

    let first = import(&pool, &doc).await;
    assert_eq!(first.results[0].status, ImportStatus::Inserted);
    assert_eq!(
        import(&pool, &doc).await.results[0].status,
        ImportStatus::Unchanged
    );

    // A change to a field outside the fingerprint is a revision of the same
    // transaction.
    let mut renamed = doc.clone();
    renamed["transactions"][0]["lines"][0]["name"] = json!("Apples (renamed)");
    let corrected = import(&pool, &renamed).await;
    assert_eq!(corrected.results[0].status, ImportStatus::Updated);
    assert_eq!(corrected.results[0].revision, 2);

    // A change to an identity field is a different purchase entirely.
    let mut retitled = renamed.clone();
    retitled["transactions"][0]["lines"][0]["original_text"] = json!("Apples 1.5kg");
    let new_purchase = import(&pool, &retitled).await;
    assert_eq!(new_purchase.results[0].status, ImportStatus::Inserted);
    assert_ne!(
        new_purchase.results[0].transaction_id,
        corrected.results[0].transaction_id
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        2
    );
}

#[tokio::test]
async fn unmatched_lines_are_preserved_verbatim() {
    let pool = setup_pool().await;
    let outcome = import(&pool, &document(Some("SRC-1"), 500)).await;

    let detail = purchase_ledger::get_transaction(&pool, HH_A, &outcome.results[0].transaction_id)
        .await
        .unwrap()
        .expect("transaction");
    assert_eq!(detail.lines.len(), 2);

    let bare = &detail.lines[1];
    assert_eq!(bare.original_text, "Mystery Item");
    assert_eq!(bare.line_total_cents, 100);
    assert_eq!(bare.match_status, "unmatched");
    assert!(bare.name.is_none() && bare.brand.is_none() && bare.canonical_item_id.is_none());
}

#[tokio::test]
async fn the_ledger_is_household_scoped() {
    let pool = setup_pool().await;
    let outcome = import(&pool, &document(Some("SRC-1"), 500)).await;
    let transaction_id = &outcome.results[0].transaction_id;

    assert!(purchase_ledger::list_transactions(&pool, HH_B)
        .await
        .unwrap()
        .is_empty());
    assert!(
        purchase_ledger::get_transaction(&pool, HH_B, transaction_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !purchase_ledger::delete_transaction(&pool, HH_B, transaction_id)
            .await
            .unwrap()
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM purchase_transactions WHERE household_id = 'hh-b'"
        )
        .await,
        0
    );
    // The other household's failed delete changed nothing.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        1
    );

    let err = purchase_ledger::import_transactions(&pool, "hh-missing", &document(None, 1))
        .await
        .expect_err("unknown household");
    assert!(matches!(err, ImportError::NoSuchHousehold(_)));
}

#[tokio::test]
async fn deleting_a_transaction_removes_its_lines_and_history() {
    let pool = setup_pool().await;
    let outcome = import(&pool, &document(Some("SRC-1"), 500)).await;
    let transaction_id = outcome.results[0].transaction_id.clone();
    import(&pool, &document(Some("SRC-1"), 600)).await; // create a revision

    assert!(
        purchase_ledger::delete_transaction(&pool, HH_A, &transaction_id)
            .await
            .unwrap()
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_lines").await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transaction_revisions").await,
        0
    );
}

#[tokio::test]
async fn a_malformed_document_reports_each_field() {
    let document = json!({
        "format_version": 1,
        "transactions": [{
            "retailer": "tesco",
            "purchase_date": "01-05-2026",
            "total_cents": 1.5,
            "extra": true,
            "lines": [{ "original_text": "x" }]
        }]
    });
    let err = purchase_ledger::parse_import_document(&document).expect_err("malformed");
    let ImportError::Malformed(errors) = err else {
        panic!("expected Malformed, got {err}");
    };
    let joined = errors.join("\n");
    for expected in [
        "transactions[0].extra: unknown field",
        "transactions[0].purchase_date: expected a YYYY-MM-DD date",
        "transactions[0].total_cents: expected an integer",
        "transactions[0].lines[0].line_total_cents: missing required field",
    ] {
        assert!(
            joined.contains(expected),
            "missing {expected:?} in:\n{joined}"
        );
    }

    let unsupported = json!({ "format_version": 2, "transactions": [] });
    assert!(matches!(
        purchase_ledger::parse_import_document(&unsupported),
        Err(ImportError::UnsupportedFormatVersion(2))
    ));
}

#[tokio::test]
async fn imports_reject_unknown_line_links_before_writing_anything() {
    let pool = setup_pool().await;
    for (field, id, message) in [
        (
            "canonical_item_id",
            "missing-item",
            "no such canonical item in this household",
        ),
        (
            "retailer_product_id",
            "missing-product",
            "no such retailer product",
        ),
    ] {
        let mut doc = document(Some("SRC-1"), 500);
        doc["transactions"][0]["lines"][0][field] = json!(id);
        if field == "canonical_item_id" {
            doc["transactions"][0]["lines"][0]["match_status"] = json!("matched");
        }
        let err = purchase_ledger::import_transactions(&pool, HH_A, &doc)
            .await
            .expect_err("unknown linked row");
        let ImportError::Malformed(errors) = err else {
            panic!("expected Malformed, got {err}");
        };
        assert!(errors.iter().any(|error| {
            error.contains(&format!("transactions[0].lines[0].{field}")) && error.contains(message)
        }));
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
            0
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM purchase_lines").await,
            0
        );
    }
}

#[tokio::test]
async fn imports_reject_canonical_items_owned_by_another_household() {
    let pool = setup_pool().await;
    seed_canonical_item(&pool, "foreign-item", HH_B).await;
    let mut doc = document(Some("SRC-1"), 500);
    doc["transactions"][0]["lines"][0]["canonical_item_id"] = json!("foreign-item");

    let err = purchase_ledger::import_transactions(&pool, HH_A, &doc)
        .await
        .expect_err("foreign canonical item");
    let ImportError::Malformed(errors) = err else {
        panic!("expected Malformed, got {err}");
    };
    assert!(errors.iter().any(|error| {
        error.contains("transactions[0].lines[0].canonical_item_id")
            && error.contains("no such canonical item in this household")
    }));
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        0
    );
}

#[tokio::test]
async fn concurrent_first_imports_share_one_transaction_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("concurrent.db");
    let db_url = format!("sqlite://{}", db_path.display());
    let pool = db::connect(&db_url).await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Home A', ?2)")
        .bind(HH_A)
        .bind(T)
        .execute(&pool)
        .await
        .expect("insert household");
    let doc = document(Some("SRC-RACE"), 500);

    let (first, second) = tokio::join!(
        purchase_ledger::import_transactions(&pool, HH_A, &doc),
        purchase_ledger::import_transactions(&pool, HH_A, &doc),
    );
    let first = first.expect("first concurrent import");
    let second = second.expect("second concurrent import");
    assert_eq!(
        first.results[0].transaction_id,
        second.results[0].transaction_id
    );
    assert_eq!(
        [first.results[0].status, second.results[0].status]
            .iter()
            .filter(|status| **status == ImportStatus::Inserted)
            .count(),
        1
    );
    assert_eq!(
        [first.results[0].status, second.results[0].status]
            .iter()
            .filter(|status| **status == ImportStatus::Unchanged)
            .count(),
        1
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
        1
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM purchase_lines").await,
        2
    );
}

#[tokio::test]
async fn the_documented_example_parses() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/purchase-ledger.md"
    ))
    .expect("read docs/purchase-ledger.md");

    const START: &str = "<!-- purchase-ledger-example:start -->";
    const END: &str = "<!-- purchase-ledger-example:end -->";
    let start = doc.find(START).expect("example start marker") + START.len();
    let end = doc[start..].find(END).expect("example end marker") + start;
    let block = &doc[start..end];
    let json_text: String = block
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");

    let value: Value = serde_json::from_str(&json_text).expect("example is valid JSON");
    let parsed = purchase_ledger::parse_import_document(&value).expect("example parses");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].lines.len(), 2);
    assert_eq!(
        parsed[0].source_transaction_id,
        parsed[1].source_transaction_id
    );
    assert_ne!(parsed[0].retailer, parsed[1].retailer);
    assert_eq!(
        parsed[0].lines[1].match_status,
        purchase_ledger::MATCH_UNMATCHED
    );
}

/// The CLI wrapper writes to a real database file and shares the core rules:
/// importing the same file twice leaves one transaction.
#[test]
fn cli_import_run_writes_into_a_database_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("buywise.db");
    let json_path = dir.path().join("purchases.json");
    let db_url = format!("sqlite://{}", db_path.display());

    // Migrate and seed the household on a private runtime, then drop it: the CLI
    // wrapper builds its own runtime, so it must not be called from inside one.
    let pool = block_on(async {
        let pool = db::connect(&db_url).await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");
        sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Home', ?2)")
            .bind(HH_A)
            .bind(T)
            .execute(&pool)
            .await
            .expect("insert household");
        pool
    });
    std::fs::write(&json_path, document(Some("SRC-1"), 500).to_string()).expect("write json");

    purchases::import_run(db_path.to_str().unwrap(), HH_A, json_path.to_str().unwrap())
        .expect("cli import");
    purchases::import_run(db_path.to_str().unwrap(), HH_A, json_path.to_str().unwrap())
        .expect("cli re-import");

    let (transactions, lines) = block_on(async {
        (
            scalar(&pool, "SELECT COUNT(*) FROM purchase_transactions").await,
            scalar(&pool, "SELECT COUNT(*) FROM purchase_lines").await,
        )
    });
    assert_eq!(transactions, 1, "re-running the CLI must not duplicate");
    assert_eq!(lines, 2);
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}
