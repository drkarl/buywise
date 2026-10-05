//! Purchase-suggestion endpoint (issue #106).
//!
//! `GET /api/list/{id}/suggestions` returns the canonical items the household is
//! likely to need soon, computed from its purchase history (see
//! [`buywise_core::suggestions`]). The list id scopes two things: authorization
//! (the caller must be a member of the owning household) and exclusion (an item
//! already active on the list is never suggested).
//!
//! The response is intentionally separate from `GET /api/list/{id}`: suggestions
//! are derived, household-scoped statistics, not list state, and the client
//! refreshes them independently of the entry snapshot. A dismissal is recorded
//! through the normal offline mutation path (`dismiss_suggestion`), so it syncs
//! and shows in the sync indicator like any other edit.
//!
//! Authorization mirrors `lists.rs`: a missing list is **404**, an existing list
//! the caller is not a member of the owning household for is **403**.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;

use buywise_core::suggestions::{compute_suggestions, Dismissal, PurchaseDay, Suggestion};

use crate::lists::is_member;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Build the purchase-suggestion routes as a `Router<AppState>`.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/list/{id}/suggestions", get(get_suggestions))
}

/// `GET /api/list/{id}/suggestions` response envelope.
#[derive(Debug, Serialize)]
pub struct SuggestionsResponse {
    /// The due items, most overdue first.
    pub suggestions: Vec<SuggestionView>,
}

/// One suggested canonical item, ready to render.
#[derive(Debug, Serialize)]
pub struct SuggestionView {
    /// `canonical_items.id`.
    pub canonical_item_id: String,
    /// `canonical_items.name_en` — the label shown in the Suggested section.
    pub display_name: String,
    /// Median gap in days between consecutive purchases.
    pub typical_interval_days: i64,
    /// Most recent purchase day, ISO-8601 date (`YYYY-MM-DD`).
    pub last_purchased_on: String,
    /// Days since the most recent purchase, as of today.
    pub days_since_last_purchase: i64,
}

/// `GET /api/list/{id}/suggestions` — compute the household's due items for one
/// list. 404 if the list does not exist; 403 if the caller is not a member of the
/// owning household.
async fn get_suggestions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<SuggestionsResponse>, StatusCode> {
    let household_id: Option<String> =
        sqlx::query_scalar("SELECT household_id FROM shopping_lists WHERE id = ?1")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_error("get_suggestions.list_household"))?;
    let household_id = household_id.ok_or(StatusCode::NOT_FOUND)?;

    if !is_member(&state.pool, &user.user_id, &household_id)
        .await
        .map_err(db_error("get_suggestions.is_member"))?
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // Purchase evidence for the household (all items at once — one scan).
    let evidence_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT canonical_item_id, occurred_at FROM purchase_events WHERE household_id = ?1",
    )
    .bind(&household_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("get_suggestions.evidence"))?;

    // Dismissals: only the most recent per item matters, so load the timestamps
    // and let the core computation pick the latest.
    let dismissal_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT canonical_item_id, dismissed_at FROM purchase_dismissals WHERE household_id = ?1",
    )
    .bind(&household_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("get_suggestions.dismissals"))?;

    // Items already active on this list are never suggested.
    let excluded: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT DISTINCT canonical_item_id FROM list_entries \
         WHERE list_id = ?1 AND status = 'active' AND canonical_item_id IS NOT NULL",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_error("get_suggestions.excluded"))?
    .into_iter()
    .collect();

    let evidence: Vec<PurchaseDay> = evidence_rows
        .iter()
        .filter_map(|(item, at)| {
            parse_day(at).map(|day| PurchaseDay {
                canonical_item_id: item.clone(),
                day,
            })
        })
        .collect();
    let dismissals: Vec<Dismissal> = dismissal_rows
        .iter()
        .filter_map(|(item, at)| {
            parse_day(at).map(|day| Dismissal {
                canonical_item_id: item.clone(),
                day,
            })
        })
        .collect();

    let today = Utc::now().date_naive();
    let suggestions = compute_suggestions(&evidence, &dismissals, &excluded, today);

    // Resolve display names for the surviving suggestions only.
    let mut views = Vec::with_capacity(suggestions.len());
    for Suggestion {
        canonical_item_id,
        typical_interval_days,
        last_purchased_on,
        days_since_last_purchase,
    } in suggestions
    {
        let display_name: Option<String> = sqlx::query_scalar(
            "SELECT name_en FROM canonical_items WHERE id = ?1 AND household_id = ?2",
        )
        .bind(&canonical_item_id)
        .bind(&household_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(db_error("get_suggestions.item_name"))?;
        let Some(display_name) = display_name else {
            // The FK guarantees the item exists; a household mismatch would mean
            // cross-tenant evidence, which the write path prevents. Skip rather
            // than leak a foreign name if it ever happened.
            continue;
        };
        views.push(SuggestionView {
            canonical_item_id,
            display_name,
            typical_interval_days,
            last_purchased_on: last_purchased_on.to_string(),
            days_since_last_purchase,
        });
    }

    Ok(Json(SuggestionsResponse { suggestions: views }))
}

/// Parse a stored ISO-8601 timestamp into its UTC calendar day. A malformed value
/// is skipped (logged) rather than failing the whole endpoint.
fn parse_day(raw: &str) -> Option<NaiveDate> {
    match DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => Some(dt.with_timezone(&Utc).date_naive()),
        Err(e) => {
            tracing::warn!(error = %e, value = raw, "suggestions: unparseable timestamp");
            None
        }
    }
}

/// Map a DB error to a logged 500, mirroring the rest of the api crate.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |e| {
        tracing::error!(error = %e, context, "suggestions: database error");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}
