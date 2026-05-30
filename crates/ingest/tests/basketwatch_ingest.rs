//! Integration tests for the BasketWatch ingest adapter.
//!
//! These exercise the full run flow against a real (in-memory) migrated SQLite
//! database, driving the adapter through [`run_ingest`] with an injected fetcher
//! so no live network is touched and timestamps stay deterministic.

use buywise_core::db;
use buywise_ingest::basketwatch::{self, FetchOutcome, RawBasketWatchProduct, RunSummary};
use buywise_ingest::error::IngestError;
use sqlx::sqlite::SqlitePool;

/// A fresh, migrated, private in-memory database (capped at one connection, so
/// the migrated schema is visible to every checkout — see `db::connect`).
async fn migrated_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Load the repository fixture the no-network path uses. Resolved relative to
/// this crate's manifest so the test is independent of the process CWD.
fn fixture_products() -> Vec<RawBasketWatchProduct> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/offers.json");
    let bytes = std::fs::read(path).expect("read fixtures/offers.json");
    serde_json::from_slice(&bytes).expect("parse fixtures/offers.json")
}

/// Run the adapter once with the fixture data at the given timestamp.
async fn ingest_fixture(pool: &SqlitePool, fetched_at: &str) -> RunSummary {
    basketwatch::run_ingest(pool, fetched_at, || async {
        Ok(FetchOutcome {
            products: fixture_products(),
            requests_made: 0,
        })
    })
    .await
    .expect("ingest should succeed")
}

async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("query")
}

#[tokio::test]
async fn no_network_run_writes_source_run_and_offers() {
    let pool = migrated_pool().await;
    let summary = ingest_fixture(&pool, "2026-05-30T10:00:00.000Z").await;

    // 9 records in the fixture; one has a null external_id and is skipped.
    assert_eq!(summary.items_fetched, 9);
    assert_eq!(summary.items_normalised, 8);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.run_number, 1, "first successful run is number 1");

    // The run row is recorded as a success with run number 1.
    let run_status: String = sqlx::query_scalar("SELECT status FROM source_runs WHERE id = ?1")
        .bind(&summary.run_id)
        .fetch_one(&pool)
        .await
        .expect("run row");
    assert_eq!(run_status, "success");

    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        8
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        8
    );

    // Every offer carries the run's number as its denormalised fetch-time copy.
    let mismatched = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM product_offers WHERE source_successful_run_number_at_fetch != 1",
    )
    .await;
    assert_eq!(mismatched, 0, "all offers stamped with run number 1");

    // The skipped (null external_id) product was never inserted.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM retailer_products WHERE name LIKE '%Mystery%'"
        )
        .await,
        0,
    );
}

#[tokio::test]
async fn brand_flows_through_ingest() {
    let pool = migrated_pool().await;
    ingest_fixture(&pool, "2026-05-30T10:00:00.000Z").await;

    let brand: Option<String> =
        sqlx::query_scalar("SELECT brand FROM retailer_products WHERE name LIKE '%Fairy%'")
            .fetch_one(&pool)
            .await
            .expect("Fairy product present");
    assert_eq!(brand.as_deref(), Some("Fairy"));
}

#[tokio::test]
async fn running_row_is_committed_before_fetch() {
    let pool = migrated_pool().await;
    let probe = pool.clone();

    let summary = basketwatch::run_ingest(&pool, "2026-05-30T10:00:00.000Z", move || async move {
        // By the time the fetcher runs, the `running` row must already be
        // durably committed (step 1 finished before step 2 begins).
        let running: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM source_runs WHERE status = 'running'")
                .fetch_one(&probe)
                .await
                .expect("probe running row");
        assert_eq!(running, 1, "running row must exist before fetch");

        Ok(FetchOutcome {
            products: fixture_products(),
            requests_made: 0,
        })
    })
    .await
    .expect("ingest");

    // After success there are no lingering `running` rows.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM source_runs WHERE status = 'running'"
        )
        .await,
        0,
    );
    assert_eq!(summary.run_number, 1);
}

#[tokio::test]
async fn second_run_dedups_products_and_advances_last_seen() {
    let pool = migrated_pool().await;
    ingest_fixture(&pool, "2026-05-30T10:00:00.000Z").await;
    let second = ingest_fixture(&pool, "2026-05-30T11:00:00.000Z").await;

    // Upsert (not REPLACE): product count is unchanged on re-ingest…
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        8
    );
    // …while the append-only offers table doubles.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        16
    );
    assert_eq!(second.run_number, 2, "second successful run is number 2");

    // first_seen_at is preserved from run 1; last_seen_at advances to run 2's
    // timestamp (deterministic because the clock is injected).
    let (first_seen, last_seen): (String, String) = sqlx::query_as(
        "SELECT first_seen_at, last_seen_at FROM retailer_products \
         WHERE name LIKE '%Fairy%'",
    )
    .fetch_one(&pool)
    .await
    .expect("Fairy product");
    assert_eq!(first_seen, "2026-05-30T10:00:00.000Z");
    assert_eq!(last_seen, "2026-05-30T11:00:00.000Z");
}

#[tokio::test]
async fn fetch_failure_marks_run_failed_with_null_run_number() {
    let pool = migrated_pool().await;

    let result = basketwatch::run_ingest(&pool, "2026-05-30T10:00:00.000Z", || async {
        Err::<FetchOutcome, _>(IngestError::MissingApiKey)
    })
    .await;
    assert!(result.is_err(), "fetch error must propagate");

    // The run is recorded as failed with no run number, and nothing was written.
    let (status, run_number): (String, Option<i64>) =
        sqlx::query_as("SELECT status, successful_run_number FROM source_runs")
            .fetch_one(&pool)
            .await
            .expect("run row");
    assert_eq!(status, "failed");
    assert_eq!(run_number, None);
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        0
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        0
    );
}

#[tokio::test]
async fn write_error_rolls_back_then_marks_failed() {
    let pool = migrated_pool().await;

    // A product whose offer violates the product_offers.loyalty_scheme CHECK
    // forces an error *after* BEGIN IMMEDIATE and after the product upsert,
    // exercising the rollback-then-mark-failed path.
    let bad = RawBasketWatchProduct {
        external_id: Some("tesco-bad".into()),
        retailer: "tesco".into(),
        name: "Bad Loyalty Scheme Product".into(),
        brand: None,
        category: None,
        url: None,
        pack_size_text: None,
        size_value: None,
        size_unit: None,
        pack_count: None,
        price_cents: 100,
        unit_price_eur_micros: None,
        unit_price_basis: None,
        previous_price_cents: None,
        loyalty_price_cents: Some(90),
        loyalty_scheme: Some("not_a_real_scheme".into()),
        offer_text: None,
        offer_valid_until: None,
        source_snapshot_date: None,
        cache_expires_at: None,
    };

    let result = basketwatch::run_ingest(&pool, "2026-05-30T10:00:00.000Z", move || async move {
        Ok(FetchOutcome {
            products: vec![bad],
            requests_made: 0,
        })
    })
    .await;
    assert!(result.is_err(), "CHECK violation must fail the run");

    // Rollback undid the product upsert; the run is marked failed (run number
    // still NULL), and the failed-mark UPDATE did not deadlock on the write lock.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        0
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        0
    );
    let (status, run_number): (String, Option<i64>) =
        sqlx::query_as("SELECT status, successful_run_number FROM source_runs")
            .fetch_one(&pool)
            .await
            .expect("run row");
    assert_eq!(status, "failed");
    assert_eq!(run_number, None);
}

/// Covers the public `ingest()` entry point with the real env-selected fetcher
/// path (not the injected-fetcher seam the other tests use): `BUYWISE_NO_NETWORK=1`
/// must read `fixtures/offers.json` and write the run + offers, with zero live
/// requests. No other test reads this env var, so toggling it here is safe.
#[tokio::test]
async fn ingest_entry_point_reads_fixture_under_no_network() {
    let pool = migrated_pool().await;

    std::env::set_var("BUYWISE_NO_NETWORK", "1");
    let summary = basketwatch::ingest(&pool).await.expect("no-network ingest");
    std::env::remove_var("BUYWISE_NO_NETWORK");

    assert_eq!(summary.items_normalised, 8);
    assert_eq!(summary.skipped, 1);
    assert_eq!(
        summary.requests_made, 0,
        "no live requests in no-network mode"
    );
    assert_eq!(summary.run_number, 1);

    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        8
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM source_runs WHERE status = 'success'"
        )
        .await,
        1
    );
}
