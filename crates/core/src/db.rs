//! SQLite connection setup and migration runner.
//!
//! Provides an async [`SqlitePool`] with foreign keys enabled, an in-process
//! migration runner backed by the repo-root `migrations/` directory, and a
//! startup assertion that SQLite's JSON1 extension is available (required by the
//! Phase-1 `list_entries` CHECK constraint and by JSON columns generally).

use std::str::FromStr;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqlitePool, SqlitePoolOptions,
};

use crate::domain::{ProductOffer, RetailerProduct, Source};

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
/// Every connection gets, per the stack decisions in `docs/design.md` §3 and the
/// pool configuration in `docs/implementation-plan.md`:
///
/// * `PRAGMA foreign_keys = ON`,
/// * `PRAGMA journal_mode = WAL` — concurrent readers alongside one writer
///   (a no-op for in-memory databases, which ignore WAL), and
/// * `PRAGMA busy_timeout = 5000` — wait up to 5 s for a lock instead of failing
///   immediately with `SQLITE_BUSY`. This matters now that the ingest adapters
///   take a manual `BEGIN IMMEDIATE` write lock: a `mark_source_run_failed`
///   short transaction (or a concurrent Lidl run) that briefly contends with a
///   held write lock waits rather than erroring spuriously.
///
/// File-backed URLs use a pool of up to 8 connections. Private in-memory URLs
/// (e.g. `sqlite::memory:`) are capped at a single connection: a wider pool
/// would give each connection its own empty database, making a migration run on
/// one connection invisible to the next checkout.
pub async fn connect(database_url: &str) -> Result<SqlitePool, DbError> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));

    let pool_options = if is_private_memory_url(database_url) {
        SqlitePoolOptions::new().max_connections(1)
    } else {
        SqlitePoolOptions::new().max_connections(8)
    };

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

// ---------------------------------------------------------------------------
// Ingest helpers (P0-T8 / P0-T9)
// ---------------------------------------------------------------------------
//
// These back the BasketWatch and Lidl adapters. The split between
// `&SqlitePool` and `&mut SqliteConnection` signatures is deliberate and
// load-bearing:
//
//   * Pool-taking helpers run in autocommit and are used OUTSIDE the success
//     transaction — committing the `running` row before any network I/O, and
//     marking a run failed in its own short transaction.
//   * Connection-taking helpers MUST run on the single connection that holds the
//     adapter's manual `BEGIN IMMEDIATE`, so the run-number allocation, product
//     upserts, and offer inserts are one atomic unit. Taking `&mut
//     SqliteConnection` (not `&SqlitePool`) makes it impossible to accidentally
//     run them in autocommit and break that atomicity.

/// Current UTC timestamp as an RFC 3339 / ISO 8601 string with a `Z` suffix
/// (e.g. `2026-05-30T12:34:56.789Z`).
///
/// Millisecond precision is used rather than seconds so that two writes in quick
/// succession (e.g. back-to-back ingest runs) are still distinguishable by
/// `last_seen_at`. Adapters that need fully deterministic timestamps (tests)
/// inject their own value instead of calling this.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Insert a `source_runs` row with `status = 'running'` and return its id.
///
/// Runs in autocommit on the pool: the row is durably committed before the
/// caller starts any network fetch, so a crash mid-fetch still leaves an
/// auditable `running` record (later reconciled to `failed`). Never call this
/// inside the success transaction.
pub async fn insert_running_source_run(
    pool: &SqlitePool,
    source: Source,
    started_at: &str,
) -> Result<String, DbError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO source_runs (id, source, started_at, status) \
         VALUES (?1, ?2, ?3, 'running')",
    )
    .bind(&id)
    .bind(source)
    .bind(started_at)
    .execute(pool)
    .await?;
    Ok(id)
}

/// The highest `successful_run_number` recorded for `source`, or `None` if no
/// run has ever succeeded.
///
/// Takes `&mut SqliteConnection` so it reads the value under the same
/// `BEGIN IMMEDIATE` lock that the subsequent `UPDATE` writes it back with — no
/// other writer can allocate the same number concurrently.
pub async fn max_successful_run_number(
    conn: &mut SqliteConnection,
    source: Source,
) -> Result<Option<i64>, DbError> {
    let max: Option<i64> =
        sqlx::query_scalar("SELECT MAX(successful_run_number) FROM source_runs WHERE source = ?1")
            .bind(source)
            .fetch_one(&mut *conn)
            .await?;
    Ok(max)
}

/// Upsert a retailer product, refreshing only `last_seen_at` on conflict.
///
/// The conflict target mirrors the partial unique index defined in
/// `migrations/001_phase0_schema.sql` exactly. **`INSERT OR REPLACE` is
/// deliberately not used**: SQLite's REPLACE is a DELETE + INSERT, which would
/// mint a new rowid and cascade-break any `product_offers.retailer_product_id`
/// foreign keys pointing at the old row. `ON CONFLICT … DO UPDATE` preserves the
/// stable primary key.
pub async fn upsert_retailer_product(
    conn: &mut SqliteConnection,
    p: &RetailerProduct,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO retailer_products \
         (id, source, retailer, external_id, url, name, brand, category, \
          pack_size_text, size_value, size_unit, pack_count, \
          first_seen_at, last_seen_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT(source, retailer, external_id) \
             WHERE source != 'manual' AND external_id IS NOT NULL \
         DO UPDATE SET last_seen_at = excluded.last_seen_at",
    )
    .bind(&p.id)
    .bind(p.source)
    .bind(p.retailer)
    .bind(&p.external_id)
    .bind(&p.url)
    .bind(&p.name)
    .bind(&p.brand)
    .bind(&p.category)
    .bind(&p.pack_size_text)
    .bind(p.size_value)
    .bind(&p.size_unit)
    .bind(p.pack_count)
    .bind(&p.first_seen_at)
    .bind(&p.last_seen_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Insert one `product_offers` row. The table is append-only, so this is always
/// an `INSERT` (never an upsert) and must run inside the success transaction.
pub async fn insert_product_offer(
    conn: &mut SqliteConnection,
    o: &ProductOffer,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO product_offers \
         (id, retailer_product_id, price_cents, unit_price_eur_micros, unit_price_basis, \
          previous_price_cents, loyalty_price_cents, loyalty_scheme, offer_text, \
          offer_valid_until, source_snapshot_date, fetched_at, cache_expires_at, \
          last_successful_source_run_id, source_successful_run_number_at_fetch, \
          source_confidence_at_ingest) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )
    .bind(&o.id)
    .bind(&o.retailer_product_id)
    .bind(o.price_cents)
    .bind(o.unit_price_eur_micros)
    .bind(o.unit_price_basis)
    .bind(o.previous_price_cents)
    .bind(o.loyalty_price_cents)
    .bind(&o.loyalty_scheme)
    .bind(&o.offer_text)
    .bind(&o.offer_valid_until)
    .bind(&o.source_snapshot_date)
    .bind(&o.fetched_at)
    .bind(&o.cache_expires_at)
    .bind(&o.last_successful_source_run_id)
    .bind(o.source_successful_run_number_at_fetch)
    .bind(o.source_confidence_at_ingest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Mark a `running` run as `success`, stamping its allocated run number, finish
/// time, and counts. Runs on the success transaction's connection.
#[allow(clippy::too_many_arguments)]
pub async fn finalise_successful_run(
    conn: &mut SqliteConnection,
    run_id: &str,
    run_number: i64,
    finished_at: &str,
    requests_made: i64,
    items_fetched: i64,
    items_normalised: i64,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE source_runs \
         SET status = 'success', successful_run_number = ?2, finished_at = ?3, \
             requests_made = ?4, items_fetched = ?5, items_normalised = ?6 \
         WHERE id = ?1",
    )
    .bind(run_id)
    .bind(run_number)
    .bind(finished_at)
    .bind(requests_made)
    .bind(items_fetched)
    .bind(items_normalised)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Mark a run `failed` with an error message, in its own short autocommit
/// transaction on the pool.
///
/// Used after the write transaction has already been rolled back (or before one
/// was ever opened), so it never competes with the success path for the same
/// connection. `successful_run_number` is left NULL.
pub async fn mark_source_run_failed(
    pool: &SqlitePool,
    run_id: &str,
    error_message: &str,
    finished_at: &str,
) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE source_runs \
         SET status = 'failed', error_message = ?2, finished_at = ?3 \
         WHERE id = ?1",
    )
    .bind(run_id)
    .bind(error_message)
    .bind(finished_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Total requests recorded against `source` for the given `month_year` (format
/// `YYYY-MM`). Drives the quota guardrails (warn / skip-non-essential /
/// hard-stop).
pub async fn current_month_request_count(
    pool: &SqlitePool,
    source: Source,
    month_year: &str,
) -> Result<i64, DbError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(requests_count), 0) FROM api_usage_events \
         WHERE source = ?1 AND month_year = ?2",
    )
    .bind(source)
    .bind(month_year)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Record a batch of API requests against the monthly quota.
pub async fn insert_api_usage_event(
    pool: &SqlitePool,
    source: Source,
    endpoint: &str,
    requests_count: i64,
    month_year: &str,
    created_at: &str,
) -> Result<(), DbError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO api_usage_events \
         (id, source, endpoint, requests_count, month_year, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&id)
    .bind(source)
    .bind(endpoint)
    .bind(requests_count)
    .bind(month_year)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Outcome of attempting to atomically reserve a single request slot against the
/// monthly budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestReservation {
    /// A slot was reserved (one `api_usage_events` request recorded). Carries the
    /// new event's id (so it can be refunded if the request never goes out) and
    /// the month-to-date total *after* this reservation.
    Granted {
        /// Primary key of the recorded `api_usage_events` row.
        event_id: String,
        /// Month-to-date request total including this reservation.
        used_after: i64,
    },
    /// The hard stop was already reached; no slot was reserved.
    Denied {
        /// Month-to-date request total at the time of denial.
        used: i64,
    },
}

/// Atomically check the monthly budget and reserve one request slot.
///
/// The count read and the reservation insert run inside a single
/// `BEGIN IMMEDIATE` transaction, so the write lock serialises concurrent ingest
/// processes: two callers cannot both observe the same stale count and each make
/// a request that, together, overshoot `hard_stop`. The loser of the lock race
/// waits (`busy_timeout`) and then re-reads the now-incremented count.
///
/// Reserving *before* the request is the conservative direction for a guardrail
/// (it can only stop early, never late). If the request then fails to reach the
/// API (no response), the caller should [`refund_request_slot`] the returned
/// `event_id` so the slot is not charged.
pub async fn reserve_request_slot(
    pool: &SqlitePool,
    source: Source,
    endpoint: &str,
    month_year: &str,
    created_at: &str,
    hard_stop: i64,
) -> Result<RequestReservation, DbError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

    // Helper so any error rolls the transaction back before propagating.
    async fn run(
        conn: &mut SqliteConnection,
        source: Source,
        endpoint: &str,
        month_year: &str,
        created_at: &str,
        hard_stop: i64,
    ) -> Result<RequestReservation, DbError> {
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(requests_count), 0) FROM api_usage_events \
             WHERE source = ?1 AND month_year = ?2",
        )
        .bind(source)
        .bind(month_year)
        .fetch_one(&mut *conn)
        .await?;

        if used >= hard_stop {
            return Ok(RequestReservation::Denied { used });
        }

        let event_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO api_usage_events \
             (id, source, endpoint, requests_count, month_year, created_at) \
             VALUES (?1, ?2, ?3, 1, ?4, ?5)",
        )
        .bind(&event_id)
        .bind(source)
        .bind(endpoint)
        .bind(month_year)
        .bind(created_at)
        .execute(&mut *conn)
        .await?;

        Ok(RequestReservation::Granted {
            event_id,
            used_after: used + 1,
        })
    }

    match run(
        &mut conn, source, endpoint, month_year, created_at, hard_stop,
    )
    .await
    {
        Ok(reservation) => {
            // A denial reserved nothing, so either COMMIT or ROLLBACK is correct;
            // ROLLBACK keeps the no-op explicit.
            let finish = match reservation {
                RequestReservation::Granted { .. } => "COMMIT",
                RequestReservation::Denied { .. } => "ROLLBACK",
            };
            sqlx::query(finish).execute(&mut *conn).await?;
            Ok(reservation)
        }
        Err(err) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(err)
        }
    }
}

/// Refund a previously reserved request slot by deleting its `api_usage_events`
/// row. The caller must only refund a request that *provably* never reached the
/// API (e.g. a pre-send connection failure); an ambiguous failure such as a
/// timeout may already have been charged by the API and must keep its slot, so
/// the monthly count never under-counts real usage.
pub async fn refund_request_slot(pool: &SqlitePool, event_id: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM api_usage_events WHERE id = ?1")
        .bind(event_id)
        .execute(pool)
        .await?;
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

    /// The monthly quota counter sums only the requested source + month, which
    /// is what drives the BasketWatch guardrails.
    #[tokio::test]
    async fn api_usage_count_sums_current_month_only() {
        let pool = fresh_pool().await;

        // Empty to start.
        assert_eq!(
            current_month_request_count(&pool, Source::BasketWatch, "2026-05")
                .await
                .expect("count"),
            0
        );

        insert_api_usage_event(
            &pool,
            Source::BasketWatch,
            "products",
            40,
            "2026-05",
            "2026-05-30T00:00:00.000Z",
        )
        .await
        .expect("insert event 1");
        insert_api_usage_event(
            &pool,
            Source::BasketWatch,
            "products",
            5,
            "2026-05",
            "2026-05-30T01:00:00.000Z",
        )
        .await
        .expect("insert event 2");
        // A different month and a different source must not be counted.
        insert_api_usage_event(
            &pool,
            Source::BasketWatch,
            "products",
            99,
            "2026-04",
            "2026-04-30T00:00:00.000Z",
        )
        .await
        .expect("insert other month");
        insert_api_usage_event(
            &pool,
            Source::LidlDirect,
            "products",
            7,
            "2026-05",
            "2026-05-30T02:00:00.000Z",
        )
        .await
        .expect("insert other source");

        assert_eq!(
            current_month_request_count(&pool, Source::BasketWatch, "2026-05")
                .await
                .expect("count"),
            45,
            "only current-month BasketWatch requests are summed"
        );
    }

    /// Reserving a slot records one request and reports the new total; once the
    /// hard stop is reached, reservations are denied without recording anything;
    /// a refund removes a previously reserved slot.
    #[tokio::test]
    async fn reserve_request_slot_enforces_hard_stop_atomically() {
        let pool = fresh_pool().await;
        let reserve = |hard_stop: i64| {
            let pool = pool.clone();
            async move {
                reserve_request_slot(
                    &pool,
                    Source::BasketWatch,
                    "products",
                    "2026-05",
                    "2026-05-30T00:00:00.000Z",
                    hard_stop,
                )
                .await
                .expect("reserve")
            }
        };

        // First two reservations are granted and increment the recorded total.
        let first = reserve(3).await;
        let second = reserve(3).await;
        assert!(
            matches!(first, RequestReservation::Granted { used_after: 1, .. }),
            "first reservation: {first:?}"
        );
        assert!(
            matches!(second, RequestReservation::Granted { used_after: 2, .. }),
            "second reservation: {second:?}"
        );
        assert_eq!(
            current_month_request_count(&pool, Source::BasketWatch, "2026-05")
                .await
                .unwrap(),
            2
        );

        // The third reservation reaches the hard stop and is granted (3 == limit
        // only denies the *next* one); the fourth is denied and records nothing.
        let third = reserve(3).await;
        assert!(matches!(
            third,
            RequestReservation::Granted { used_after: 3, .. }
        ));
        let denied = reserve(3).await;
        assert_eq!(denied, RequestReservation::Denied { used: 3 });
        assert_eq!(
            current_month_request_count(&pool, Source::BasketWatch, "2026-05")
                .await
                .unwrap(),
            3,
            "a denied reservation must not record a request"
        );

        // Refunding the first slot decrements the recorded total.
        let RequestReservation::Granted { event_id, .. } = first else {
            unreachable!("first was granted");
        };
        refund_request_slot(&pool, &event_id).await.expect("refund");
        assert_eq!(
            current_month_request_count(&pool, Source::BasketWatch, "2026-05")
                .await
                .unwrap(),
            2,
            "refund removes exactly the refunded slot"
        );
    }
}
