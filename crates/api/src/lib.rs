//! BuyWise HTTP API library (axum 0.8).
//!
//! Phase 1 exposes the authentication routes (P1-T2) under `/api/auth`, the
//! list-management routes (P1-T3) under `/api/lists`, `/api/list/:id`, and
//! `/api/items`, the offline-sync mutation endpoint (P1-T4) at `/api/mutations`,
//! the Server-Sent Events stream (P1-T5) at `/api/sse/:list_id`, and a Phase-4
//! placeholder for the basket optimiser (P4-T1) at `/api/optimise`. The shared
//! [`AppState`] carries the SQLite pool that handlers and the
//! [`middleware::AuthenticatedUser`] extractor read from, plus the per-list SSE
//! broadcast registry.
//!
//! The full application is assembled by [`app`] (the API routes plus the static
//! SvelteKit SPA served from `frontend/build`), while [`api_router`] builds just
//! the API surface so integration tests can mount the exact same routes against
//! an in-memory database without binding a socket or depending on a built
//! frontend. Both are kept separate from the binary entry point.

use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::any;
use axum::Router;
use dashmap::DashMap;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};

/// Default location of the built SvelteKit SPA, relative to the server's working
/// directory. Produced by `npm run build` in `frontend/` (adapter-static, SPA
/// mode); does not exist until Phase 2, so `GET /` 404s until then.
pub const FRONTEND_BUILD_DIR: &str = "frontend/build";

pub mod auth;
pub mod lists;
pub mod llm;
pub mod middleware;
pub mod mutations;
pub mod optimise;
pub mod resolution;
pub mod settings;
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
/// the broadcast registry. [`sse::SseConfig`] is small (three `Copy` fields) and
/// cloned by value.
#[derive(Clone)]
pub struct AppState {
    /// The SQLite connection pool.
    pub pool: SqlitePool,
    /// Per-list SSE broadcast senders (P1-T5).
    pub sse_channels: SseChannels,
    /// Tunable SSE parameters (P1-T5). Defaults to production values; integration
    /// tests override the heartbeat interval / broadcast buffer / retention
    /// cutoff to exercise timing- and capacity-dependent paths deterministically.
    pub sse_config: sse::SseConfig,
    /// LLM client for the resolution cascade's step-3 fallback (P3-T2). Built from
    /// the environment by [`AppState::new`] (disabled by default — see
    /// [`llm::LlmConfig::from_env`]); integration tests inject a fake via
    /// [`AppState::with_llm_client`] so they never touch the network.
    pub llm_client: Arc<dyn llm::LlmClient>,
}

impl AppState {
    /// Build state for a ready pool with an empty SSE channel registry, the
    /// production [`sse::SseConfig`] defaults, and an env-configured LLM client
    /// (disabled unless `LLM_FALLBACK_ENABLED=true`).
    pub fn new(pool: SqlitePool) -> Self {
        AppState {
            pool,
            sse_channels: Arc::new(DashMap::new()),
            sse_config: sse::SseConfig::default(),
            llm_client: llm::client_from_env(),
        }
    }

    /// Override the SSE configuration (used by tests to force timing/capacity
    /// paths without waiting for real-world intervals).
    pub fn with_sse_config(mut self, config: sse::SseConfig) -> Self {
        self.sse_config = config;
        self
    }

    /// Override the LLM client (used by tests to inject a fake transport so the
    /// step-3 fallback runs deterministically without a network call).
    pub fn with_llm_client(mut self, client: Arc<dyn llm::LlmClient>) -> Self {
        self.llm_client = client;
        self
    }
}

/// Build just the API router (every `/api/...` route) from a ready [`AppState`].
///
/// Kept separate from [`app`] so integration tests can exercise the handlers
/// without a built frontend on disk. The static-file fallback lives in [`app`].
pub fn api_router(state: AppState) -> Router {
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
        // `optimise::router` carries the absolute `/api/optimise` path (P4-T1).
        .merge(optimise::router())
        // `settings::router` carries the absolute `/api/settings` path: the
        // household's configurable convenience thresholds (P4-T2).
        .merge(settings::router())
        // Catch-all 404 for any unmatched `/api/...` path. Without this, the SPA
        // `fallback_service` in `app` would swallow unknown API URLs once
        // `frontend/build/200.html` exists and serve the HTML shell with a `200`,
        // masking client typos / removed endpoints as if they were app routes.
        // Static routes above are matched before this wildcard (matchit gives
        // literal segments priority over a catch-all), and a known path hit with
        // the wrong method still yields the route's own `405` — only genuinely
        // unrouted `/api/...` paths reach this handler. The bare `/api` path needs
        // its own route: `{*rest}` requires at least one trailing segment, so
        // without this `GET /api` would fall through to the SPA shell.
        .route("/api", any(api_not_found))
        .route("/api/{*rest}", any(api_not_found))
        .with_state(state)
}

/// Fallback handler for unmatched `/api/...` paths: always `404 Not Found`, so
/// the API surface never falls through to the SPA shell (see [`api_router`]).
async fn api_not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Build the top-level application: the API routes plus the SvelteKit SPA served
/// from [`FRONTEND_BUILD_DIR`].
///
/// API routes take precedence; any other path falls through to [`ServeDir`],
/// which serves a matching static file or, for client-side routes, the SPA shell
/// (`200.html`, produced by SvelteKit's `adapter-static` in SPA mode). Until the
/// frontend is built (Phase 2) the directory is absent, so unmatched paths 404.
pub fn app(state: AppState) -> Router {
    app_with_frontend(state, FRONTEND_BUILD_DIR)
}

/// Like [`app`], but with the static-asset directory chosen by the caller.
///
/// This is a test seam: it is `pub` only so integration tests can point the
/// static fallback at a fixture build directory. Production code uses [`app`].
#[doc(hidden)]
pub fn app_with_frontend(state: AppState, frontend_dir: impl AsRef<Path>) -> Router {
    let dir = frontend_dir.as_ref();
    // Serve files from the build dir; for any path without a matching file fall
    // back to the SPA shell so client-side routing works on hard reloads/deep
    // links. The fallback file (and dir) may not exist yet — `ServeDir` returns
    // 404 in that case rather than panicking.
    let spa_shell = dir.join("200.html");
    let serve_dir = ServeDir::new(dir).fallback(ServeFile::new(spa_shell));
    api_router(state).fallback_service(serve_dir)
}
