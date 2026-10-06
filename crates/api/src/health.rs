//! `GET /api/health` — unauthenticated readiness probe (issue #141).
//!
//! A deploy needs a check that is stronger than "a listener answered": the
//! endpoint reports whether the SQLite database is reachable and whether every
//! migration embedded in this binary has been applied. It is deliberately
//! shallow — it never touches household data — and returns only a short status,
//! so it is safe to expose without a session and without a CSRF token.
//!
//! Readiness, not liveness: a failing check means the process is answering but
//! cannot serve correct data, and the response is `503 Service Unavailable` so a
//! load balancer or the `deploy/smoke.sh` probe treats it as a failure. The
//! failure detail goes to the journal only, never into the body.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::AppState;

/// Migrations embedded at compile time from the repo-root `migrations/`
/// directory — the same set `buywise_core::db::run_migrations` applies at
/// startup. Comparing against them is what makes "migrations are applied" a real
/// check rather than a "the table exists" guess.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Build the health route as a `Router<AppState>`, merged by [`crate::api_router`].
///
/// Registered in the API router but outside the [`crate::middleware::AuthenticatedUser`]
/// extractor, so it needs neither a session cookie nor a CSRF token.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/health", get(health))
}

/// The response body: one short status field and nothing else. No database path,
/// no migration names, no versions — none of which an unauthenticated caller
/// should see.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// Readiness handler: `200 {"status":"ok"}` when the database answers and every
/// embedded migration is applied, otherwise `503 {"status":"unavailable"}`.
///
/// The reason is logged (journal) and never returned, so the public body cannot
/// leak schema or path detail.
async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    match check(&state.pool).await {
        Ok(()) => (StatusCode::OK, Json(HealthResponse { status: "ok" })),
        Err(reason) => {
            tracing::error!(error = %reason, "health check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(HealthResponse {
                    status: "unavailable",
                }),
            )
        }
    }
}

/// `Ok(())` when the pool answers a trivial query and every migration the binary
/// was built with is recorded as applied.
///
/// The error string is for the journal only.
async fn check(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("database unreachable: {e}"))?;

    // A missing `_sqlx_migrations` table (an unmigrated database) is a database
    // error here, which correctly reports the schema as not ready.
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("migration state unreadable: {e}"))?;

    let pending: Vec<i64> = MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .filter(|version| !applied.contains(version))
        .collect();
    if !pending.is_empty() {
        return Err(format!("unapplied migrations: {pending:?}"));
    }

    Ok(())
}
