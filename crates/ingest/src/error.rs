//! Error type for the ingest adapters.
//!
//! [`IngestError`] is the failure mode for a *whole run* — a network failure, a
//! malformed payload, a database error, or hitting the hard quota stop. It is
//! distinct from a *per-product* problem such as a missing `external_id`: those
//! are logged and the offending product is skipped (see
//! [`crate::basketwatch::normalise`]), so they never abort an otherwise healthy
//! run.

/// A fatal error for an ingest run.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// A live HTTP request to the source API failed.
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),

    /// A payload (live response or fixture file) could not be parsed as JSON.
    #[error("failed to parse source payload: {0}")]
    Json(#[from] serde_json::Error),

    /// Reading the fixture file (no-network mode) failed.
    #[error("failed to read fixture {path}: {source}")]
    Fixture {
        /// The fixture path we tried to read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A database operation failed (via a core db helper).
    #[error("db helper error: {0}")]
    Db(#[from] buywise_core::db::DbError),

    /// A raw sqlx operation failed (e.g. the manual `BEGIN IMMEDIATE` /
    /// `COMMIT` / `ROLLBACK` statements the adapter issues directly).
    #[error("transaction control error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// Live mode was requested but `BASKETWATCH_API_KEY` is unset.
    #[error("BASKETWATCH_API_KEY is not set (required for live ingest)")]
    MissingApiKey,

    /// Live Lidl mode was requested but `LIDL_FEED_URL` is unset. There is no
    /// verified default Lidl.ie feed (design §16 open question), so the endpoint
    /// must be configured explicitly. This is raised *before* a `source_runs` row
    /// is opened, so a misconfigured deployment fails fast rather than recording a
    /// `failed` run against a known-bad default URL on every invocation.
    #[error("LIDL_FEED_URL is not set (required for live Lidl ingest)")]
    MissingFeedUrl,

    /// A source payload was syntactically valid but could not be interpreted as
    /// the expected product listing (e.g. the Lidl.ie page/feed structure
    /// changed, or a required field is absent). Distinct from [`Json`](Self::Json)
    /// — that is a *syntax* failure; this is a *shape* failure.
    #[error("unexpected source payload shape: {0}")]
    Parse(String),

    /// The monthly request budget hard-stop (4,900) has been reached; no live
    /// calls are permitted until the next month.
    #[error("API hard-stop reached: {used} requests used this month (limit 4900)")]
    QuotaHardStop {
        /// Requests already recorded for the current month.
        used: i64,
    },
}
