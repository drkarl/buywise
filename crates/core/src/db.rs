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

/// Open a pooled connection to `database_url`.
///
/// For file-backed URLs the database is created if missing. `PRAGMA
/// foreign_keys = ON` is applied to every connection in the pool.
pub async fn connect(database_url: &str) -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;
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
        .map_err(|_| DbError::Json1Unavailable)?;

    if length != 0 {
        return Err(DbError::Json1Unavailable);
    }
    Ok(())
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
        let canonical = object_sql(&pool, "table", "canonical_items")
            .await
            .expect("canonical_items DDL");
        assert!(canonical.contains("CHECK"));
        assert!(canonical.contains("'own_label'"));

        let aliases = object_sql(&pool, "table", "item_aliases")
            .await
            .expect("item_aliases DDL");
        assert!(aliases.contains("language IN ('en','es')"));
        assert!(aliases.contains("source IN ('manual','accepted_match','llm')"));

        let constraints = object_sql(&pool, "table", "item_constraints")
            .await
            .expect("item_constraints DDL");
        assert!(constraints.contains("substitution_policy IN ('any','ask','strict')"));

        let runs = object_sql(&pool, "table", "source_runs")
            .await
            .expect("source_runs DDL");
        assert!(runs.contains("status IN ('running','success','failed')"));

        let products = object_sql(&pool, "table", "retailer_products")
            .await
            .expect("retailer_products DDL");
        assert!(products.contains("retailer IN ('tesco','dunnes','supervalu','aldi','lidl')"));

        let offers = object_sql(&pool, "table", "product_offers")
            .await
            .expect("product_offers DDL");
        assert!(offers.contains("source_confidence_at_ingest IN ('high','medium','low')"));

        let matches = object_sql(&pool, "table", "product_matches")
            .await
            .expect("product_matches DDL");
        assert!(matches.contains("status IN ('accepted','rejected','suggested','pending')"));
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
}
