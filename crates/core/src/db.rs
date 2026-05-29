//! SQLite connection setup and migration runner.
//!
//! Provides an async [`SqlitePool`] with foreign keys enabled, an in-process
//! migration runner backed by the repo-root `migrations/` directory, and a
//! startup assertion that SQLite's JSON1 extension is available (required by the
//! Phase-1 `list_entries` CHECK constraint and by JSON columns generally).

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Errors produced while connecting to, migrating, or validating the database.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A connection, query, or migration failed at the sqlx layer.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    /// A migration could not be applied or validated.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// SQLite was built without the JSON1 extension, which the schema requires.
    #[error("SQLite JSON1 extension unavailable")]
    Json1Unavailable,
}

/// True for URLs that resolve to a private (non-shared-cache) in-memory database.
///
/// Each connection to `sqlite::memory:` (or `sqlite://:memory:`) opens its OWN
/// independent database. With a multi-connection pool, a migration applied on
/// one connection would be invisible to queries that later acquire a different,
/// empty connection. We special-case these URLs to a single connection so the
/// schema is shared across all pool checkouts.
///
/// URLs that opt into a shared cache (`cache=shared`) genuinely share one
/// in-memory database across connections and are left untouched.
fn is_private_memory_url(database_url: &str) -> bool {
    let lower = database_url.to_ascii_lowercase();
    let is_memory = lower.contains(":memory:") || lower.contains("mode=memory");
    is_memory && !lower.contains("cache=shared")
}

/// Open a pooled connection to `database_url`.
///
/// For file-backed URLs the database is created if missing and the pool may hold
/// multiple connections. `PRAGMA foreign_keys = ON` is applied to every
/// connection in the pool.
///
/// Private in-memory URLs (e.g. `sqlite::memory:`) are capped at a single
/// connection: a wider pool would give each connection its own empty database,
/// making a migration run on one connection invisible to the next checkout.
pub async fn connect(database_url: &str) -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true);

    let mut pool_options = SqlitePoolOptions::new();
    if is_private_memory_url(database_url) {
        pool_options = pool_options.max_connections(1);
    }

    let pool = pool_options.connect_with(options).await?;
    Ok(pool)
}

/// Apply all pending migrations from the repo-root `migrations/` directory.
///
/// The path is relative to this crate (`crates/core`). sqlx embeds the files at
/// compile time and records applied versions in `_sqlx_migrations`, so re-running
/// is idempotent.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), DbError> {
    sqlx::migrate!("../../migrations").run(pool).await?;
    Ok(())
}

/// Assert that SQLite's JSON1 extension is available.
///
/// Runs `SELECT json_array_length('[]')`; if the function is missing (i.e. the
/// SQLite build lacks JSON1) this returns [`DbError::Json1Unavailable`].
pub async fn assert_json1(pool: &SqlitePool) -> Result<(), DbError> {
    let length: i64 = sqlx::query_scalar("SELECT json_array_length('[]')")
        .fetch_one(pool)
        .await
        .map_err(map_json1_error)?;

    if length != 0 {
        return Err(DbError::Json1Unavailable);
    }
    Ok(())
}

/// Classify the error from the JSON1 probe query.
///
/// Only a genuine "no such function" database error means JSON1 is missing; any
/// other failure (closed pool, I/O error, ...) is propagated unchanged via
/// [`DbError::Sqlx`] rather than being misreported as JSON1-unavailable.
fn map_json1_error(err: sqlx::Error) -> DbError {
    if let Some(db_err) = err.as_database_error() {
        if db_err.message().contains("no such function") {
            return DbError::Json1Unavailable;
        }
    }
    DbError::Sqlx(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table or index name expected in `sqlite_master` after migrating.
    async fn object_sql(pool: &SqlitePool, kind: &str, name: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
        )
        .bind(kind)
        .bind(name)
        .fetch_optional(pool)
        .await
        .expect("query sqlite_master")
        .flatten()
    }

    async fn fresh_pool() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.expect("connect in-memory");
        run_migrations(&pool).await.expect("run migrations");
        pool
    }

    #[tokio::test]
    async fn migration_creates_all_nine_tables() {
        let pool = fresh_pool().await;

        let expected = [
            "categories",
            "canonical_items",
            "item_aliases",
            "item_constraints",
            "source_runs",
            "retailer_products",
            "product_offers",
            "product_matches",
            "api_usage_events",
        ];

        for name in expected {
            let sql = object_sql(&pool, "table", name).await;
            assert!(sql.is_some(), "table `{name}` should exist after migration");
        }

        // Exactly the nine application tables (excluding sqlx bookkeeping).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name != '_sqlx_migrations'",
        )
        .fetch_one(&pool)
        .await
        .expect("count tables");
        assert_eq!(count, 9, "exactly nine Phase-0 tables expected");
    }

    #[tokio::test]
    async fn partial_unique_indexes_have_correct_predicates() {
        let pool = fresh_pool().await;

        let source_external = object_sql(&pool, "index", "uq_retailer_products_source_external")
            .await
            .expect("source/external index should exist");
        assert!(source_external.contains("UNIQUE"));
        assert!(
            source_external.contains("WHERE source != 'manual' AND external_id IS NOT NULL"),
            "retailer_products partial index predicate missing: {source_external}"
        );

        let run_number = object_sql(&pool, "index", "uq_source_runs_source_run_number")
            .await
            .expect("source_runs run-number index should exist");
        assert!(run_number.contains("UNIQUE"));
        assert!(
            run_number.contains("WHERE successful_run_number IS NOT NULL"),
            "source_runs partial index predicate missing: {run_number}"
        );

        // The other named indexes also exist.
        assert!(
            object_sql(&pool, "index", "uq_item_aliases_household_alias_lang")
                .await
                .is_some()
        );
        assert!(
            object_sql(&pool, "index", "uq_product_matches_household_item_product")
                .await
                .is_some()
        );
        assert!(
            object_sql(&pool, "index", "idx_product_offers_product_fetched")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn check_constraints_are_present() {
        let pool = fresh_pool().await;

        // CHECK constraints are stored inline in the table DDL in sqlite_master.
        // Assert the *full* enum list per column so a narrowed enum is caught.
        let canonical = object_sql(&pool, "table", "canonical_items")
            .await
            .expect("canonical_items DDL");
        assert!(canonical.contains("CHECK"));
        assert!(canonical.contains(
            "default_constraint_policy IN\n              \
             ('any','brand','allowed_set','own_label','exact','ask')"
        ));

        let aliases = object_sql(&pool, "table", "item_aliases")
            .await
            .expect("item_aliases DDL");
        assert!(aliases.contains("language IN ('en','es')"));
        assert!(aliases.contains("source IN ('manual','accepted_match','llm')"));

        let constraints = object_sql(&pool, "table", "item_constraints")
            .await
            .expect("item_constraints DDL");
        assert!(constraints.contains("substitution_policy IN ('any','ask','strict')"));
        assert!(constraints.contains("preferred_size_basis IN ('g','ml','each')"));
        assert!(constraints.contains("min_size_basis IN ('g','ml','each')"));
        assert!(constraints.contains("max_size_basis IN ('g','ml','each')"));

        let runs = object_sql(&pool, "table", "source_runs")
            .await
            .expect("source_runs DDL");
        assert!(runs.contains("status IN ('running','success','failed')"));
        assert!(runs.contains("source IN ('basketwatch','lidl_direct','manual')"));

        let products = object_sql(&pool, "table", "retailer_products")
            .await
            .expect("retailer_products DDL");
        assert!(products.contains("retailer IN ('tesco','dunnes','supervalu','aldi','lidl')"));
        assert!(products.contains("source IN ('basketwatch','lidl_direct','manual')"));

        let offers = object_sql(&pool, "table", "product_offers")
            .await
            .expect("product_offers DDL");
        assert!(offers.contains("source_confidence_at_ingest IN ('high','medium','low')"));
        assert!(offers.contains("loyalty_scheme IN ('clubcard','real_rewards')"));

        let matches = object_sql(&pool, "table", "product_matches")
            .await
            .expect("product_matches DDL");
        assert!(matches.contains("status IN ('accepted','rejected','suggested','pending')"));
        assert!(matches.contains("matched_by IN ('user','system','llm')"));
    }

    #[tokio::test]
    async fn run_number_column_is_append_only() {
        let pool = fresh_pool().await;

        // No DEFAULT clause on the denormalised run-number column.
        let offers = object_sql(&pool, "table", "product_offers")
            .await
            .expect("product_offers DDL");
        let line = offers
            .lines()
            .find(|l| l.contains("source_successful_run_number_at_fetch"))
            .expect("column present");
        assert!(
            !line.to_uppercase().contains("DEFAULT"),
            "append-only column must have no DEFAULT: {line}"
        );

        // No triggers exist at all (append-only design).
        let trigger_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger'")
                .fetch_one(&pool)
                .await
                .expect("count triggers");
        assert_eq!(trigger_count, 0, "no triggers expected in Phase-0 schema");
    }

    #[tokio::test]
    async fn migration_is_idempotent() {
        let pool = fresh_pool().await;
        // Re-running against the same pool must be a no-op (version table guards it).
        run_migrations(&pool).await.expect("re-run migrations");
    }

    #[tokio::test]
    async fn json1_extension_is_available() {
        let pool = connect("sqlite::memory:").await.expect("connect in-memory");
        assert_json1(&pool)
            .await
            .expect("JSON1 should be available");
    }

    /// M1: with a private in-memory URL the pool must be capped at one
    /// connection, so the schema applied during migration is visible to every
    /// subsequent checkout. Acquire several connections and query a migrated
    /// table on each to prove the schema is shared.
    #[tokio::test]
    async fn in_memory_schema_visible_across_connections() {
        let pool = fresh_pool().await;

        // Repeatedly acquire connections and query a migrated table. If the
        // pool opened independent in-memory DBs, one of these would hit an
        // empty connection and fail with "no such table".
        // `categories` is seeded by 002_seed.sql with 11 rows; a consistent
        // non-zero count on every checkout proves the migrated+seeded schema is
        // shared (an independent empty DB would return 0 or "no such table").
        for _ in 0..5 {
            let mut conn = pool.acquire().await.expect("acquire connection");
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM categories")
                .fetch_one(&mut *conn)
                .await
                .expect("categories table visible on this connection");
            assert_eq!(count, 11);
        }

        // The same must hold when querying through the pool directly. The seed
        // does not touch product_offers, so it stays empty.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM product_offers")
            .fetch_one(&pool)
            .await
            .expect("schema visible through pool");
        assert_eq!(count, 0);
    }

    /// M3: foreign keys are enabled per connection; inserting a child row that
    /// references a non-existent parent must fail.
    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let pool = fresh_pool().await;

        // PRAGMA reports enforcement is on for this connection.
        let fk_on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("query PRAGMA foreign_keys");
        assert_eq!(fk_on, 1, "foreign key enforcement should be enabled");

        // product_offers.retailer_product_id references a missing retailer product.
        let result = sqlx::query(
            "INSERT INTO product_offers \
             (id, retailer_product_id, price_cents, fetched_at, source_confidence_at_ingest) \
             VALUES ('offer-1', 'does-not-exist', 100, '2026-01-01T00:00:00Z', 'high')",
        )
        .execute(&pool)
        .await;

        let err = result.expect_err("insert with dangling FK must fail");
        assert!(
            err.to_string().to_lowercase().contains("foreign key"),
            "expected foreign key violation, got: {err}"
        );
    }

    /// L2: file-backed URLs exercise `create_if_missing` (never hit by the
    /// in-memory tests). Use a unique temp path with no extra crate dependency
    /// and clean it up afterwards.
    #[tokio::test]
    async fn file_backed_database_is_created_and_migrated() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("buywise-test-{}-{nanos}.db", std::process::id()));
        assert!(!path.exists(), "temp db should not pre-exist");

        let url = format!("sqlite://{}", path.display());
        let pool = connect(&url).await.expect("connect file-backed");
        run_migrations(&pool)
            .await
            .expect("run migrations on file db");

        assert!(path.exists(), "create_if_missing should create the file");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name != '_sqlx_migrations'",
        )
        .fetch_one(&pool)
        .await
        .expect("count tables");
        assert_eq!(count, 9, "nine tables in the file-backed database");

        pool.close().await;

        // Clean up the temp file (and any -wal/-shm sidecars). Verified to live
        // under the system temp dir.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
