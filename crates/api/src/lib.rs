//! BuyWise HTTP API library (axum 0.8).
//!
//! Phase 1 exposes the authentication routes (P1-T2) under `/api/auth` and the
//! list-management routes (P1-T3) under `/api/lists`, `/api/list/:id`, and
//! `/api/items`. The shared [`AppState`] carries the SQLite pool that handlers
//! and the [`middleware::AuthenticatedUser`] extractor read from.
//!
//! The router is built by [`app`], kept separate from the binary entry point so
//! integration tests can mount the exact same routes against an in-memory
//! database without binding a socket.

use axum::Router;
use sqlx::SqlitePool;

pub mod auth;
pub mod lists;
pub mod middleware;

/// Application state shared across handlers and extractors.
///
/// `Clone` is cheap: [`SqlitePool`] is an `Arc` internally, so cloning shares the
/// same connection pool.
#[derive(Clone)]
pub struct AppState {
    /// The SQLite connection pool.
    pub pool: SqlitePool,
}

/// Build the top-level application router from a ready [`AppState`].
pub fn app(state: AppState) -> Router {
    // Warm the lazily-computed dummy password hash so the first unknown-user
    // login does not pay a one-time argon2 cost (which would briefly reintroduce
    // an account-existence timing signal).
    auth::warm_dummy_hash();
    Router::new()
        .nest("/api/auth", auth::router())
        // `lists::router` already carries absolute `/api/...` paths (the list
        // endpoints span two path shapes — `/api/lists` and `/api/list/:id` —
        // so they are merged rather than nested under one prefix).
        .merge(lists::router())
        .with_state(state)
}
