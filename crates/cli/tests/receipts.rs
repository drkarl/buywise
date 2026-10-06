//! Integration tests for receipt-image expiry (issue #149, roadmap #114).
//!
//! The sweep's clock is injected, so every assertion is deterministic: images
//! are stored at fixed instants and the sweep runs at another fixed instant, with
//! no wall clock in the comparison. The last test drives the operator command
//! (`buywise-cli expire-receipts`) end to end against a real database file.
//!
//! Pinned acceptance criteria: the sweep removes files and records past 30 days
//! and leaves newer ones; running it twice changes nothing; a file that is
//! already gone is tolerated (the record still goes); and the command's image
//! directory is `--receipt-dir`, else `RECEIPT_DIR`, else beside the database.

use std::path::Path;
use std::sync::Mutex;

use buywise_core::db;
use buywise_core::receipts::{self, ReceiptConfig};
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;

const HH: &str = "hh-a";
const USER: &str = "u-a";

/// A minimal PNG: enough signature for the upload path to accept it.
fn image_bytes(padding: usize) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.resize(bytes.len() + padding, 0x00);
    bytes
}

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .expect("parse instant")
        .with_timezone(&Utc)
}

/// The sweep's fixed "now": 2026-02-15T00:00:00Z.
fn sweep_now() -> DateTime<Utc> {
    at("2026-02-15T00:00:00Z")
}

async fn setup_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Home', ?2)")
        .bind(HH)
        .bind(db::now_iso())
        .execute(&pool)
        .await
        .expect("household");
    sqlx::query(
        "INSERT INTO users (id, email, password_hash, display_name, created_at) \
         VALUES (?1, 'a@example.test', 'hash', 'Ann', ?2)",
    )
    .bind(USER)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("user");
    sqlx::query(
        "INSERT INTO household_members (household_id, user_id, role, joined_at) \
         VALUES (?1, ?2, 'owner', ?3)",
    )
    .bind(HH)
    .bind(USER)
    .bind(db::now_iso())
    .execute(&pool)
    .await
    .expect("membership");
    pool
}

/// Store one image uploaded at `uploaded_at`; returns its id.
async fn store(
    pool: &SqlitePool,
    config: &ReceiptConfig,
    uploaded_at: DateTime<Utc>,
) -> receipts::ReceiptImage {
    receipts::store_receipt(
        pool,
        config,
        HH,
        USER,
        "local",
        &image_bytes(16),
        uploaded_at,
    )
    .await
    .expect("store receipt")
}

async fn count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM receipt_images WHERE household_id = ?1")
        .bind(HH)
        .fetch_one(pool)
        .await
        .expect("count")
}

/// An image stored 45 days before the sweep is expired and removed (file first,
/// then record); one stored 5 days before it is untouched. A second run changes
/// nothing.
#[tokio::test]
async fn the_sweep_removes_expired_files_and_records_and_keeps_fresh_ones() {
    let pool = setup_pool().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let config = ReceiptConfig::with_dir(temp.path());

    let expired = store(&pool, &config, sweep_now() - Duration::days(45)).await;
    let fresh = store(&pool, &config, sweep_now() - Duration::days(5)).await;
    // 30-day retention: the first expired 15 days ago, the second expires in 25.
    assert!(expired.expires_at < receipts::iso(sweep_now()));
    assert!(fresh.expires_at > receipts::iso(sweep_now()));
    let expired_file = temp.path().join(&expired.storage_key);
    let fresh_file = temp.path().join(&fresh.storage_key);
    assert!(expired_file.exists() && fresh_file.exists());

    let report = receipts::expire_receipts(&pool, temp.path(), sweep_now())
        .await
        .expect("sweep");
    assert_eq!(report.expired_records, 1);
    assert_eq!(report.records_removed, 1);
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.files_already_gone, 0);
    assert!(!expired_file.exists(), "the expired image must be gone");
    assert!(fresh_file.exists(), "a fresh image must survive");
    assert_eq!(count(&pool).await, 1);

    // Safe to run repeatedly: the second sweep finds nothing.
    let again = receipts::expire_receipts(&pool, temp.path(), sweep_now())
        .await
        .expect("second sweep");
    assert_eq!(again, receipts::ExpiryReport::default());
    assert_eq!(count(&pool).await, 1);
    assert!(fresh_file.exists());
}

/// A record whose file is already gone is still removed, and the sweep reports
/// it rather than failing.
#[tokio::test]
async fn the_sweep_tolerates_a_file_that_is_already_gone() {
    let pool = setup_pool().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let config = ReceiptConfig::with_dir(temp.path());

    let expired = store(&pool, &config, sweep_now() - Duration::days(31)).await;
    std::fs::remove_file(temp.path().join(&expired.storage_key)).expect("remove by hand");

    let report = receipts::expire_receipts(&pool, temp.path(), sweep_now())
        .await
        .expect("sweep");
    assert_eq!(report.expired_records, 1);
    assert_eq!(report.records_removed, 1);
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.files_already_gone, 1);
    assert_eq!(count(&pool).await, 0);
}

/// The exact 30-day boundary: a record expiring at the sweep instant is removed,
/// one expiring a millisecond later is kept.
#[tokio::test]
async fn the_sweep_uses_the_thirty_day_boundary() {
    let pool = setup_pool().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let config = ReceiptConfig::with_dir(temp.path());

    let on_boundary = store(&pool, &config, sweep_now() - Duration::days(30)).await;
    let just_inside = store(
        &pool,
        &config,
        sweep_now() - Duration::days(30) + Duration::milliseconds(1),
    )
    .await;

    let report = receipts::expire_receipts(&pool, temp.path(), sweep_now())
        .await
        .expect("sweep");
    assert_eq!(report.records_removed, 1);
    let remaining = receipts::list_receipts(&pool, HH, sweep_now())
        .await
        .expect("list");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, just_inside.id);
    assert!(!temp.path().join(&on_boundary.storage_key).exists());
}

/// The operator command sweeps a real database file, deleting the expired image
/// and leaving the fresh one alone.
#[test]
fn the_expire_command_sweeps_a_database_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("buywise.db");
    let db_path = db_path.to_str().expect("utf-8 path").to_string();
    let receipt_dir = temp.path().join("receipts");
    let receipt_dir_arg = receipt_dir.to_str().expect("utf-8 path").to_string();

    let pool = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            let pool = db::connect(&format!("sqlite://{db_path}"))
                .await
                .expect("connect");
            db::run_migrations(&pool).await.expect("migrate");
            sqlx::query("INSERT INTO households (id, name, created_at) VALUES (?1, 'Home', ?2)")
                .bind(HH)
                .bind(db::now_iso())
                .execute(&pool)
                .await
                .expect("household");
            sqlx::query(
                "INSERT INTO users (id, email, password_hash, display_name, created_at) \
                 VALUES (?1, 'a@example.test', 'hash', 'Ann', ?2)",
            )
            .bind(USER)
            .bind(db::now_iso())
            .execute(&pool)
            .await
            .expect("user");
            sqlx::query(
                "INSERT INTO household_members (household_id, user_id, role, joined_at) \
                 VALUES (?1, ?2, 'owner', ?3)",
            )
            .bind(HH)
            .bind(USER)
            .bind(db::now_iso())
            .execute(&pool)
            .await
            .expect("membership");

            let config = ReceiptConfig::with_dir(&receipt_dir);
            // One image already past its retention, one uploaded just now.
            let expired = receipts::store_receipt(
                &pool,
                &config,
                HH,
                USER,
                "local",
                &image_bytes(16),
                Utc::now() - Duration::days(31),
            )
            .await
            .expect("store expired");
            let fresh = receipts::store_receipt(
                &pool,
                &config,
                HH,
                USER,
                "local",
                &image_bytes(16),
                Utc::now(),
            )
            .await
            .expect("store fresh");
            (pool, expired, fresh)
        });
    let (pool, expired, fresh) = pool;

    buywise_cli::commands::receipts::expire_run(&db_path, Some(&receipt_dir_arg))
        .expect("expire run");

    assert!(
        !receipt_dir.join(&expired.storage_key).exists(),
        "the expired image must be gone"
    );
    assert!(
        receipt_dir.join(&fresh.storage_key).exists(),
        "the fresh image must survive"
    );
    let remaining: Vec<String> = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(async {
            sqlx::query_scalar("SELECT id FROM receipt_images WHERE household_id = ?1")
                .bind(HH)
                .fetch_all(&pool)
                .await
        })
        .expect("rows");
    assert_eq!(remaining, vec![fresh.id.clone()]);

    // A database that does not exist is refused, like the other data commands.
    let err = buywise_cli::commands::receipts::expire_run("/nonexistent/buywise.db", None)
        .expect_err("a missing database must fail");
    assert!(err.to_string().contains("does not exist"), "got {err}");
}

/// Serialises the one test that reads and writes `RECEIPT_DIR`, since the
/// process environment is shared.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The image directory is `--receipt-dir`, else `RECEIPT_DIR`, else a `receipts/`
/// directory beside the database — the deployment contract the systemd unit
/// relies on.
#[test]
fn the_receipt_directory_defaults_beside_the_database() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let previous = std::env::var(receipts::ENV_RECEIPT_DIR).ok();
    std::env::remove_var(receipts::ENV_RECEIPT_DIR);

    let beside = ReceiptConfig::for_database("/var/lib/buywise/buywise.db", None);
    assert_eq!(beside.dir, Path::new("/var/lib/buywise/receipts"));

    std::env::set_var(receipts::ENV_RECEIPT_DIR, "/srv/receipts");
    let from_env = ReceiptConfig::for_database("/var/lib/buywise/buywise.db", None);
    assert_eq!(from_env.dir, Path::new("/srv/receipts"));

    let overridden = ReceiptConfig::for_database("/var/lib/buywise/buywise.db", Some("/mnt/img"));
    assert_eq!(overridden.dir, Path::new("/mnt/img"));

    // The documented default limits survive an unparsable override.
    std::env::set_var(receipts::ENV_MAX_IMAGE_BYTES, "not-a-number");
    assert_eq!(
        ReceiptConfig::from_env().max_image_bytes,
        receipts::DEFAULT_MAX_IMAGE_BYTES
    );
    std::env::remove_var(receipts::ENV_MAX_IMAGE_BYTES);

    match previous {
        Some(value) => std::env::set_var(receipts::ENV_RECEIPT_DIR, value),
        None => std::env::remove_var(receipts::ENV_RECEIPT_DIR),
    }
}
