//! Basket-optimisation route (`POST /api/optimise`).
//!
//! Placeholder for Phase 4 (P4-T1), which will run the optimiser over a user's
//! list and return the four basket plans. Wiring the route now keeps the public
//! API surface stable and lets the P1-T6 router/integration tests assert its
//! presence; until P4-T1 lands it responds `501 Not Implemented`.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;

use crate::AppState;

/// Build the optimise route as a `Router<AppState>`, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/api/optimise", post(optimise_stub))
}

/// Phase-1 placeholder handler: the optimiser endpoint is implemented in P4-T1.
// TODO(P4-T1): require an `AuthenticatedUser` extractor (and CSRF guard) here, as
// on every other `/api/...` route. Omitted now because the body is a static 501
// with no state access and the auth contract is finalised in Phase 4; wiring the
// extractor against an empty handler would be dead code until then.
async fn optimise_stub() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "POST /api/optimise is not yet implemented (arrives in Phase 4)",
    )
}
