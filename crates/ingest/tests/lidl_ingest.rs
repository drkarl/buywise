//! Integration tests for the Lidl.ie direct ingest adapter (P0-T9).
//!
//! These exercise the full run flow against a real (in-memory) migrated SQLite
//! database, driving the adapter through [`run_ingest`] with an injected fetcher
//! so no live network is touched and timestamps stay deterministic. They also
//! cover the isolation contract: a Lidl failure (or panic) must be swallowed by
//! [`spawn_ingest`] rather than propagated to the caller.

use std::sync::Mutex;

use buywise_core::db;
use buywise_ingest::error::IngestError;
use buywise_ingest::lidl::{self, FetchOutcome, RawLidlProduct, RunSummary};
use sqlx::sqlite::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serialises the two tests that mutate process-global ingest env vars
/// (`BUYWISE_NO_NETWORK` / `LIDL_FEED_URL`); cargo runs tests in a binary
/// concurrently, so without this they could clobber each other's settings.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A fresh, migrated, private in-memory database (capped at one connection, so
/// the migrated schema is visible to every checkout — see `db::connect`).
async fn migrated_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// Load the repository fixture the no-network path uses. Resolved relative to
/// this crate's manifest so the test is independent of the process CWD.
fn fixture_products() -> Vec<RawLidlProduct> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/lidl_offers.json"
    );
    let bytes = std::fs::read(path).expect("read fixtures/lidl_offers.json");
    serde_json::from_slice(&bytes).expect("parse fixtures/lidl_offers.json")
}

/// Run the adapter once with the fixture data at the given timestamp.
async fn ingest_fixture(pool: &SqlitePool, fetched_at: &str) -> RunSummary {
    lidl::run_ingest(pool, fetched_at, || async {
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

    // 7 records in the fixture; one has a null external_id and is skipped.
    assert_eq!(summary.items_fetched, 7);
    assert_eq!(summary.items_normalised, 6);
    assert_eq!(summary.skipped, 1);
    assert_eq!(
        summary.run_number, 1,
        "first successful Lidl run is number 1"
    );

    // The run row is recorded as a success against the Lidl source.
    let (status, source): (String, String) =
        sqlx::query_as("SELECT status, source FROM source_runs WHERE id = ?1")
            .bind(&summary.run_id)
            .fetch_one(&pool)
            .await
            .expect("run row");
    assert_eq!(status, "success");
    assert_eq!(source, "lidl_direct", "Lidl gets its own source_runs rows");

    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        6
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        6
    );

    // Every Lidl offer is stamped medium confidence (acceptance criterion).
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM product_offers WHERE source_confidence_at_ingest = 'medium'"
        )
        .await,
        6
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM product_offers WHERE source_confidence_at_ingest != 'medium'"
        )
        .await,
        0
    );

    // Every retailer product is the Lidl retailer / lidl_direct source.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM retailer_products \
             WHERE retailer = 'lidl' AND source = 'lidl_direct'"
        )
        .await,
        6
    );
}

#[tokio::test]
async fn pack_size_extracted_from_name() {
    let pool = migrated_pool().await;
    ingest_fixture(&pool, "2026-05-30T10:00:00.000Z").await;

    // "2L" is embedded in the milk product's name; parse_pack_size pulls it out.
    let pack: Option<String> = sqlx::query_scalar(
        "SELECT pack_size_text FROM retailer_products WHERE name LIKE '%Semi Skimmed Milk%'",
    )
    .fetch_one(&pool)
    .await
    .expect("milk product present");
    assert_eq!(pack.as_deref(), Some("2000ml"));

    // And the offer's unit price was computed from that size (€1.99 / 2L).
    let unit: Option<i64> = sqlx::query_scalar(
        "SELECT o.unit_price_eur_micros FROM product_offers o \
         JOIN retailer_products p ON p.id = o.retailer_product_id \
         WHERE p.name LIKE '%Semi Skimmed Milk%'",
    )
    .fetch_one(&pool)
    .await
    .expect("milk offer present");
    assert_eq!(unit, Some(995_000));
}

#[tokio::test]
async fn second_run_dedups_products_and_advances_last_seen() {
    let pool = migrated_pool().await;
    ingest_fixture(&pool, "2026-05-30T10:00:00.000Z").await;
    let second = ingest_fixture(&pool, "2026-05-30T11:00:00.000Z").await;

    // Upsert (not REPLACE): product count is unchanged on re-ingest…
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        6
    );
    // …while the append-only offers table doubles.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        12
    );
    assert_eq!(
        second.run_number, 2,
        "second successful Lidl run is number 2"
    );

    let (first_seen, last_seen): (String, String) = sqlx::query_as(
        "SELECT first_seen_at, last_seen_at FROM retailer_products \
         WHERE name LIKE '%Semi Skimmed Milk%'",
    )
    .fetch_one(&pool)
    .await
    .expect("milk product");
    assert_eq!(first_seen, "2026-05-30T10:00:00.000Z");
    assert_eq!(last_seen, "2026-05-30T11:00:00.000Z");
}

#[tokio::test]
async fn running_row_is_committed_before_fetch() {
    let pool = migrated_pool().await;
    let probe = pool.clone();

    lidl::run_ingest(&pool, "2026-05-30T10:00:00.000Z", move || async move {
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM source_runs WHERE status = 'running' AND source = 'lidl_direct'",
        )
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

    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM source_runs WHERE status = 'running'"
        )
        .await,
        0,
    );
}

/// A parse/shape error from the fetcher must be recorded as a `failed`
/// `source_run` (not a panic), with nothing written. This is the
/// "parse errors produce status='failed'" testing expectation.
#[tokio::test]
async fn parse_error_marks_run_failed_without_panic() {
    let pool = migrated_pool().await;

    let result = lidl::run_ingest(&pool, "2026-05-30T10:00:00.000Z", || async {
        Err::<FetchOutcome, _>(IngestError::Parse(
            "Lidl.ie response was not a product listing".into(),
        ))
    })
    .await;
    assert!(
        result.is_err(),
        "parse error must propagate from run_ingest"
    );

    let (status, run_number): (String, Option<i64>) =
        sqlx::query_as("SELECT status, successful_run_number FROM source_runs")
            .fetch_one(&pool)
            .await
            .expect("run row");
    assert_eq!(status, "failed");
    assert_eq!(run_number, None, "failed run has no run number");
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        0
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        0
    );
}

/// Lidl runs are independent of BasketWatch: a Lidl run allocates its own run
/// number sequence and does not touch BasketWatch's run rows.
#[tokio::test]
async fn lidl_runs_are_independent_of_basketwatch() {
    let pool = migrated_pool().await;

    // A pre-existing successful BasketWatch run at number 5.
    let bw_id = db::insert_running_source_run(
        &pool,
        buywise_core::Source::BasketWatch,
        "2026-05-29T09:00:00.000Z",
    )
    .await
    .expect("insert bw running");
    {
        let mut conn = pool.acquire().await.expect("acquire");
        db::finalise_successful_run(&mut conn, &bw_id, 5, "2026-05-29T09:05:00.000Z", 0, 0, 0)
            .await
            .expect("finalise bw");
    }

    // Lidl's first run is still number 1 — it does not inherit BasketWatch's 5.
    let summary = ingest_fixture(&pool, "2026-05-30T10:00:00.000Z").await;
    assert_eq!(
        summary.run_number, 1,
        "Lidl has its own run-number sequence"
    );

    // BasketWatch's run row is untouched.
    let bw_number: Option<i64> =
        sqlx::query_scalar("SELECT successful_run_number FROM source_runs WHERE id = ?1")
            .bind(&bw_id)
            .fetch_one(&pool)
            .await
            .expect("bw run row");
    assert_eq!(bw_number, Some(5));
}

/// The isolation entry point: `spawn_ingest` must swallow a Lidl failure. We
/// force a failure by pointing the no-network fixture path at a *missing* file
/// (closing the env var window the moment the task starts), then await the
/// handle — it resolves to `()` with no error and the parent is unaffected.
// Holds `ENV_LOCK` across `.await` to serialise the env-var mutation against the
// other env-sensitive test. The lock only guards a process-global env var (not
// shared async state), the test runtime is single-threaded, and the guard is
// dropped before the assertions — so the cross-task deadlock this lint guards
// against cannot occur here.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn spawn_ingest_swallows_failure() {
    let pool = migrated_pool().await;

    let guard = ENV_LOCK.lock().unwrap();
    // Force live mode with an unreachable feed so the fetch fails fast. The
    // adapter still records a `failed` source_run, but the error is swallowed by
    // the spawned task rather than surfacing here.
    std::env::set_var(
        "LIDL_FEED_URL",
        "http://127.0.0.1:1/definitely-not-listening",
    );
    std::env::remove_var("BUYWISE_NO_NETWORK");

    let handle = lidl::spawn_ingest(pool.clone());
    // The detached task completes without propagating the error: awaiting it
    // yields Ok(()) (no JoinError, no panic), proving the failure was contained.
    handle.await.expect("spawned task must not panic or abort");

    std::env::remove_var("LIDL_FEED_URL");
    drop(guard);

    // A failed Lidl source_run was recorded — the failure was handled, not lost.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM source_runs WHERE source = 'lidl_direct' AND status = 'failed'"
        )
        .await,
        1,
    );
    // Nothing was written to the product tables.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        0
    );
}

/// Covers the public `ingest()` entry point with the env-selected fetcher path:
/// `BUYWISE_NO_NETWORK=1` must read `fixtures/lidl_offers.json` and write the run
/// + offers, with zero live requests.
// See `spawn_ingest_swallows_failure` for why holding the lock across `.await`
// is sound here.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn ingest_entry_point_reads_fixture_under_no_network() {
    let pool = migrated_pool().await;

    let guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("BUYWISE_NO_NETWORK", "1");
    let summary = lidl::ingest(&pool).await.expect("no-network ingest");
    std::env::remove_var("BUYWISE_NO_NETWORK");
    drop(guard);

    assert_eq!(summary.items_normalised, 6);
    assert_eq!(summary.skipped, 1);
    assert_eq!(
        summary.requests_made, 0,
        "no live requests in no-network mode"
    );
    assert_eq!(summary.run_number, 1);

    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM source_runs WHERE status = 'success' AND source = 'lidl_direct'"
        )
        .await,
        1
    );
}

/// Spawn a one-shot HTTP/1.1 server that replies to the first connection with a
/// `200 OK` carrying `body`, then returns its `http://127.0.0.1:<port>/` URL.
///
/// Used to drive the *real* `live_fetch` path against a controlled response so
/// the genuine deserialisation-shape → `IngestError::Parse` conversion runs (the
/// injected-fetcher seam can't exercise that conversion).
async fn one_shot_server(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            // Drain the request so the client's write side does not error.
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://{addr}/")
}

/// Drives the real `live_fetch` against a server that returns a syntactically
/// valid 2xx JSON body of the *wrong shape* (an object, not a product array).
/// The genuine `response.json::<Vec<_>>()` failure must be converted to
/// `IngestError::Parse` and recorded as a `failed` source_run — no panic. This
/// covers the real conversion the injected-`Parse` test cannot reach.
// See `spawn_ingest_swallows_failure` for why holding the lock across `.await`
// is sound here.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn live_fetch_shape_mismatch_marks_run_failed_without_panic() {
    let pool = migrated_pool().await;
    // A 2xx body that parses as JSON but not as `Vec<RawLidlProduct>`.
    let url = one_shot_server("{\"error\":\"unexpected\"}").await;

    let guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("BUYWISE_NO_NETWORK");
    std::env::set_var("LIDL_FEED_URL", &url);
    let result = lidl::ingest(&pool).await;
    std::env::remove_var("LIDL_FEED_URL");
    drop(guard);

    match result {
        Err(IngestError::Parse(_)) => {}
        other => panic!("expected IngestError::Parse from a shape mismatch, got {other:?}"),
    }

    // The run was recorded as failed, with nothing written to the product tables.
    let (status, run_number): (String, Option<i64>) = sqlx::query_as(
        "SELECT status, successful_run_number FROM source_runs WHERE source = 'lidl_direct'",
    )
    .fetch_one(&pool)
    .await
    .expect("run row");
    assert_eq!(status, "failed");
    assert_eq!(run_number, None, "failed run has no run number");
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM retailer_products").await,
        0
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM product_offers").await,
        0
    );
}
