//! The match-feedback write path — `POST /api/match-feedback` (P5-T1, issue #32).
//!
//! Lets a user accept, reject, or prefer a specific product match surfaced in the
//! optimiser plan view. Each call does two writes inside one transaction:
//!
//!   1. **Upsert `product_matches`** keyed by the
//!      `(household_id, canonical_item_id, retailer_product_id)` unique triple via
//!      `INSERT ... ON CONFLICT ... DO UPDATE` so there is exactly one preference
//!      row per triple (AC #4). `INSERT OR REPLACE` is deliberately NOT used: it
//!      deletes-then-reinserts, which would change the row's `id` and break the
//!      `match_feedback.product_match_id` FK references that point at it.
//!   2. **Insert a `match_feedback` row** recording the action verbatim. The
//!      feedback table is append-only history (AC #3): every action is preserved,
//!      so accepting then rejecting the same match leaves one `product_matches`
//!      row (status `rejected`) and two `match_feedback` rows.
//!
//! The `product_match_id` written into `match_feedback` is the id of the row the
//! upsert produced (`RETURNING id`) — for an update that is the pre-existing row's
//! id (preserving FK references), for an insert the freshly generated id.
//!
//! Action → status mapping:
//!   * `accept`  → status `accepted`
//!   * `reject`  → status `rejected`
//!   * `prefer`  → status `accepted`. The schema has no dedicated `preferred`
//!     status; `prefer` is a stronger accept (the user actively chose this product
//!     over alternatives), so it maps to `accepted` for routing while the
//!     `match_feedback.action` keeps the verbatim `prefer` so the signal is not
//!     lost from history.
//!   * `undo`    → status `pending` (issue #108). Unlike the others, `undo`
//!     corrects a decision that already exists: it requires a `product_matches`
//!     row for the triple (a 404 otherwise) and returns it to the neutral
//!     undecided state, so the product is no longer auto-priced and is proposed
//!     again as a candidate. The prior accept/reject rows are NOT deleted — an
//!     `undo` feedback row is appended, keeping the trail append-only.
//!
//! Authorisation: the request body carries no `household_id`. Rather than assume a
//! single household (the limitation `settings.rs` documents), the household is
//! derived from the `canonical_item_id` itself — `canonical_items` are
//! household-scoped — and the caller must be a member of THAT household. An unknown
//! canonical item, or one in a household the caller does not belong to, is a 404
//! (never distinguishing "wrong household" from "absent", so a caller cannot probe
//! another household's catalogue). The `retailer_product_id` must also reference a
//! real `retailer_products` row (retailer products are global, not
//! household-scoped) so the upsert never writes a dangling FK.
//!
//! Error discipline mirrors the sibling handlers (`mutations.rs`, `settings.rs`): a
//! DB error is logged and surfaced as 500 (never failing open); the
//! [`AuthenticatedUser`] extractor enforces 401 (no session) and 403 (missing/bad
//! CSRF, this is a POST); an unknown `action` is 422; an unknown canonical item /
//! non-member is 404; an unknown retailer product is 404.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, Transaction};

use buywise_core::db;

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the match-feedback route as a `Router<AppState>`, merged by [`crate::app`].
pub fn router() -> Router<AppState> {
    Router::new().route("/api/match-feedback", post(post_match_feedback))
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// Body for `POST /api/match-feedback`.
///
/// No `household_id`: it is derived from `canonical_item_id` (household-scoped) and
/// the caller's membership is checked against it (see the module docs).
#[derive(Debug, Deserialize)]
pub struct MatchFeedbackRequest {
    /// The canonical (catalogue) item the match is for.
    pub canonical_item_id: String,
    /// The retailer product the match binds to.
    pub retailer_product_id: String,
    /// `accept`, `reject`, or `prefer` (validated at the boundary).
    pub action: String,
}

/// Response for `POST /api/match-feedback`: the resulting match row id and its
/// status after the upsert, plus the verbatim action recorded in history.
#[derive(Debug, Serialize)]
pub struct MatchFeedbackResponse {
    /// `product_matches.id` of the upserted row (stable across updates).
    pub product_match_id: String,
    /// The match's `status` after applying this feedback (`accepted`|`rejected`).
    pub status: String,
    /// The action recorded verbatim in `match_feedback` (`accept`|`reject`|`prefer`).
    pub action: String,
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

/// Map a DB error to a logged 500, mirroring `mutations::db_error` / `settings`.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "match_feedback: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// The `product_matches.status` an action maps to, or `None` for an unknown action
/// (a 422 at the boundary). `match_feedback.action` keeps the verbatim string; only
/// the routing status is derived here. See the module docs for the `prefer` choice.
///
/// `undo` resets a decided match to `pending` — the product then counts as
/// undecided, is no longer auto-priced, and is proposed again as a candidate.
/// It is the deliberate reset that lets a rejected suggestion recur (issue #108);
/// unlike the other actions it requires an existing `product_matches` row.
fn status_for_action(action: &str) -> Option<&'static str> {
    match action {
        "accept" => Some("accepted"),
        "reject" => Some("rejected"),
        // `prefer` is a stronger accept — no dedicated status exists, so route it
        // as `accepted` while the verbatim `prefer` survives in match_feedback.
        "prefer" => Some("accepted"),
        "undo" => Some("pending"),
        _ => None,
    }
}

/// The `product_matches.matched_by` an action is attributed to. A user's
/// accept/reject/prefer is `user`; an `undo` returns the row to the neutral
/// `system` state (no product has been matched), while the `match_feedback` row
/// still records which user performed the undo.
fn matched_by_for_action(action: &str) -> &'static str {
    if action == "undo" {
        "system"
    } else {
        "user"
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/match-feedback` — record an accept/reject/prefer on a product match.
///
/// Upserts the `product_matches` row (status in place) and appends a
/// `match_feedback` row, atomically. See the module docs for the full contract.
async fn post_match_feedback(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<MatchFeedbackRequest>,
) -> Result<(StatusCode, Json<MatchFeedbackResponse>), StatusCode> {
    // 0. Validate the action discriminator at the boundary (422 for anything else).
    let status = status_for_action(&req.action).ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;

    let pool = &state.pool;

    // 1. Authorisation: derive the household from the canonical item (catalogue is
    //    household-scoped) and require the caller to be a member. An unknown item or
    //    one in another household is a 404 — never leaking which it was (H4-style).
    let household_id = canonical_item_household(pool, &req.canonical_item_id)
        .await
        .map_err(db_error("post_match_feedback.canonical_household"))?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !is_member(pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("post_match_feedback.is_member"))?
    {
        return Err(StatusCode::NOT_FOUND);
    }

    // 2. The retailer product must exist (retailer products are global). Without
    //    this the upsert could write a dangling `retailer_product_id` FK.
    if !retailer_product_exists(pool, &req.retailer_product_id)
        .await
        .map_err(db_error("post_match_feedback.retailer_product"))?
    {
        return Err(StatusCode::NOT_FOUND);
    }

    // 3. `undo` corrects existing state, so it requires a match to undo. Accept/
    //    reject/prefer may introduce one (they are the first decision on the
    //    triple). An undo of a never-decided product is a 404, never a silent
    //    insert of a pending row.
    if req.action == "undo"
        && !product_match_exists(
            pool,
            &household_id,
            &req.canonical_item_id,
            &req.retailer_product_id,
        )
        .await
        .map_err(db_error("post_match_feedback.undo_lookup"))?
    {
        return Err(StatusCode::NOT_FOUND);
    }

    // 4. Upsert the match + append the feedback row, atomically.
    let mut tx = pool
        .begin()
        .await
        .map_err(db_error("post_match_feedback.begin"))?;

    let matched_by = matched_by_for_action(&req.action);
    let product_match_id = upsert_product_match(&mut tx, &household_id, &req, status, matched_by)
        .await
        .map_err(db_error("post_match_feedback.upsert"))?;

    insert_match_feedback(&mut tx, &product_match_id, &user.user_id, &req.action)
        .await
        .map_err(db_error("post_match_feedback.feedback"))?;

    tx.commit()
        .await
        .map_err(db_error("post_match_feedback.commit"))?;

    Ok((
        StatusCode::CREATED,
        Json(MatchFeedbackResponse {
            product_match_id,
            status: status.to_string(),
            action: req.action,
        }),
    ))
}

// ---------------------------------------------------------------------------
// DB helpers
// ---------------------------------------------------------------------------

/// The household owning a canonical item, or `None` if no such item exists.
/// `canonical_items` are household-scoped, so this both resolves the household and
/// proves the item exists.
async fn canonical_item_household(
    pool: &sqlx::SqlitePool,
    canonical_item_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT household_id FROM canonical_items WHERE id = ?1")
        .bind(canonical_item_id)
        .fetch_optional(pool)
        .await
}

/// True if `user_id` is a member of `household_id` (mirrors `mutations::is_member`).
async fn is_member(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    household_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM household_members WHERE user_id = ?1 AND household_id = ?2",
    )
    .bind(user_id)
    .bind(household_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// True if a `retailer_products` row with this id exists (products are global, not
/// household-scoped), so the upsert never writes a dangling FK.
async fn retailer_product_exists(
    pool: &sqlx::SqlitePool,
    retailer_product_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> = sqlx::query_scalar("SELECT 1 FROM retailer_products WHERE id = ?1")
        .bind(retailer_product_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// True if a `product_matches` row already exists for the triple. Used to gate
/// `undo` (there must be a decision to correct); accept/reject/prefer do not need
/// it because the upsert may legitimately introduce the first row.
async fn product_match_exists(
    pool: &sqlx::SqlitePool,
    household_id: &str,
    canonical_item_id: &str,
    retailer_product_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM product_matches \
         WHERE household_id = ?1 AND canonical_item_id = ?2 AND retailer_product_id = ?3",
    )
    .bind(household_id)
    .bind(canonical_item_id)
    .bind(retailer_product_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Upsert the `product_matches` row for the unique
/// `(household_id, canonical_item_id, retailer_product_id)` triple and return its
/// id. `ON CONFLICT ... DO UPDATE` updates `status`/`matched_by`/`updated_at` in
/// place (NOT `INSERT OR REPLACE`, which would delete-and-reinsert and orphan the
/// `match_feedback.product_match_id` FK). `RETURNING id` yields the surviving row's
/// id on both the insert and the update path, so it is the correct FK to record in
/// `match_feedback`.
///
/// `matched_by` is `user` for a user-driven match decision and `system` for an
/// `undo`, which returns the row to the neutral undecided state (see
/// [`matched_by_for_action`]). On insert both `created_at` and `updated_at` are
/// stamped to now; on update only `updated_at` is bumped (the original
/// `created_at` is preserved by not being in the `DO UPDATE` set).
async fn upsert_product_match(
    tx: &mut Transaction<'_, Sqlite>,
    household_id: &str,
    req: &MatchFeedbackRequest,
    status: &str,
    matched_by: &str,
) -> Result<String, sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = db::now_iso();
    sqlx::query_scalar(
        "INSERT INTO product_matches \
             (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, \
              created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
         ON CONFLICT(household_id, canonical_item_id, retailer_product_id) DO UPDATE SET \
             status = excluded.status, \
             matched_by = excluded.matched_by, \
             updated_at = excluded.updated_at \
         RETURNING id",
    )
    .bind(&id)
    .bind(household_id)
    .bind(&req.canonical_item_id)
    .bind(&req.retailer_product_id)
    .bind(status)
    .bind(matched_by)
    .bind(&now)
    .fetch_one(&mut **tx)
    .await
}

/// Append a `match_feedback` row recording the verbatim action against the upserted
/// `product_matches` row. Append-only history (AC #3): never updated, so the full
/// accept/reject/prefer trail is preserved.
async fn insert_match_feedback(
    tx: &mut Transaction<'_, Sqlite>,
    product_match_id: &str,
    user_id: &str,
    action: &str,
) -> Result<(), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = db::now_iso();
    sqlx::query(
        "INSERT INTO match_feedback (id, product_match_id, user_id, action, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(&id)
    .bind(product_match_id)
    .bind(user_id)
    .bind(action)
    .bind(&now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_for_action_maps_known_actions() {
        assert_eq!(status_for_action("accept"), Some("accepted"));
        assert_eq!(status_for_action("reject"), Some("rejected"));
        // `prefer` is a stronger accept (see the module docs).
        assert_eq!(status_for_action("prefer"), Some("accepted"));
    }

    #[test]
    fn status_for_action_rejects_unknown_action() {
        assert_eq!(status_for_action("explode"), None);
        assert_eq!(status_for_action(""), None);
        assert_eq!(status_for_action("ACCEPT"), None);
    }
}
