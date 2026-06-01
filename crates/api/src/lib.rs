//! BuyWise HTTP API library (axum 0.8).
//!
//! Phase 1 exposes the authentication routes (P1-T2) under `/api/auth`, the
//! list-management routes (P1-T3) under `/api/lists`, `/api/list/:id`, and
//! `/api/items`, the offline-sync mutation endpoint (P1-T4) at `/api/mutations`,
//! and the Server-Sent Events stream (P1-T5) at `/api/sse/:list_id`. The shared
//! [`AppState`] carries the SQLite pool that handlers and the
//! [`middleware::AuthenticatedUser`] extractor read from, plus the per-list SSE
//! broadcast registry.
//!
//! The router is built by [`app`], kept separate from the binary entry point so
//! integration tests can mount the exact same routes against an in-memory
//! database without binding a socket.

use std::sync::Arc;

use axum::Router;
use dashmap::DashMap;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

pub mod auth;
pub mod lists;
pub mod middleware;
pub mod mutations;
pub mod sse;

/// Registry of per-list SSE broadcast senders, keyed by `list_id`.
///
/// Wrapped in an `Arc` so cloning [`AppState`] (cheap, per-request) shares one
/// map across all handlers. A channel is created lazily on the first subscriber
/// for a list (see [`sse::sender_for`]) and the mutation commit path looks one up
/// to fan a committed mutation out to every connected client.
pub type SseChannels = Arc<DashMap<String, broadcast::Sender<sse::SseEvent>>>;

/// Application state shared across handlers and extractors.
///
/// `Clone` is cheap: [`SqlitePool`] is an `Arc` internally and the SSE channel
/// registry is an `Arc<DashMap>`, so cloning shares both the connection pool and
/// the broadcast registry.
#[derive(Clone)]
pub struct AppState {
    /// The SQLite connection pool.
    pub pool: SqlitePool,
    /// Per-list SSE broadcast senders (P1-T5).
    pub sse_channels: SseChannels,
}

impl AppState {
    /// Build state for a ready pool with an empty SSE channel registry.
    pub fn new(pool: SqlitePool) -> Self {
        AppState {
            pool,
            sse_channels: Arc::new(DashMap::new()),
        }
    }
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
        // `mutations::router` carries the absolute `/api/mutations` path; merged
        // like the list routes (P1-T4).
        .merge(mutations::router())
        // `sse::router` carries the absolute `/api/sse/{list_id}` path (P1-T5).
        .merge(sse::router())
        .with_state(state)
}
