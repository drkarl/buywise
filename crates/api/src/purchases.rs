//! Household purchase-ledger endpoints (issue #148, roadmap #114).
//!
//! The purchase ledger records what the household actually bought: one row per
//! transaction and one per line, imported from a retailer export today and from
//! a read receipt later. The logic lives in [`buywise_core::purchase_ledger`],
//! shared with the CLI's `import-purchases` command so the two cannot drift; the
//! import format is documented in [`docs/purchase-ledger.md`](../../docs/purchase-ledger.md).
//!
//! | Route | Method | Who | Purpose |
//! | --- | --- | --- | --- |
//! | `/api/household/{id}/purchases` | `POST` | any member | import one or many transactions |
//! | `/api/household/{id}/purchases` | `GET` | any member | list the household's transactions |
//! | `/api/household/{id}/purchases/{txid}` | `GET` | any member | one transaction with its lines and revisions |
//! | `/api/household/{id}/purchases/{txid}` | `DELETE` | any member | delete one transaction (lines + revisions cascade) |
//!
//! # Authorisation
//!
//! The household id is explicit in the path and every handler requires the
//! caller to be a member of *that* household (`household_members`), mirroring
//! `crates/api/src/household.rs`: a member of another household is refused with
//! 403, so one household can never read, import into or delete another's
//! ledger. A transaction id that belongs to another household reads and deletes
//! as if it did not exist (404), because every core query filters on the
//! household id.
//!
//! # Import errors and size
//!
//! Malformed input is refused with 422 and a JSON body `{ "errors": [...] }`
//! whose entries name the offending field path (e.g.
//! `transactions[0].lines[1].line_total_cents: expected an integer`), so an
//! importer can be fixed from the response. A `format_version` this build does
//! not read is also 422. The route is capped at
//! [`MAX_IMPORT_BODY_BYTES`] so one oversized body can never exhaust the server.

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use buywise_core::purchase_ledger::{self, ImportError, TransactionDetail, TransactionSummary};
use serde_json::{json, Value};

use crate::middleware::AuthenticatedUser;
use crate::AppState;

/// Largest request body accepted by the import route (1 MiB). The ledger format
/// is compact JSON; a household import of many transactions fits comfortably,
/// and the limit bounds the memory one request can allocate. The core parser
/// additionally caps a document at
/// [`MAX_TRANSACTIONS`](buywise_core::purchase_ledger::MAX_TRANSACTIONS) and
/// each transaction at
/// [`MAX_LINES_PER_TRANSACTION`](buywise_core::purchase_ledger::MAX_LINES_PER_TRANSACTION).
pub const MAX_IMPORT_BODY_BYTES: usize = 1024 * 1024;

/// Build the purchase-ledger routes as a `Router<AppState>`, merged by
/// [`crate::api_router`]. Paths are absolute, like the sibling modules.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/household/{household_id}/purchases",
            post(import_purchases).get(list_purchases),
        )
        .route(
            "/api/household/{household_id}/purchases/{transaction_id}",
            get(get_purchase).delete(delete_purchase),
        )
        // The import is the only route that reads a request body; cap it here so
        // the limit travels with the route rather than the whole app.
        .layer(DefaultBodyLimit::max(MAX_IMPORT_BODY_BYTES))
}

/// `POST /api/household/{household_id}/purchases` — import one document.
///
/// 403 unless the caller is a member of `household_id`; 404 if the household
/// does not exist; 422 with `{ "errors": [...] }` for malformed input or an
/// unsupported `format_version`. On success returns an
/// [`ImportOutcome`](buywise_core::purchase_ledger::ImportOutcome): per-transaction
/// insert/update/unchanged results and the totals.
async fn import_purchases(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Json(document): Json<Value>,
) -> Result<Response, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;

    match purchase_ledger::import_transactions(&state.pool, &household_id, &document).await {
        Ok(outcome) => Ok((StatusCode::OK, Json(outcome)).into_response()),
        Err(ImportError::Malformed(errors)) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "errors": errors })),
        )
            .into_response()),
        Err(ImportError::UnsupportedFormatVersion(version)) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "errors": [format!(
                    "$.format_version: unsupported version {version}; this build reads version {}",
                    purchase_ledger::IMPORT_FORMAT_VERSION
                )],
            })),
        )
            .into_response()),
        Err(ImportError::NoSuchHousehold(_)) => Err(StatusCode::NOT_FOUND),
        Err(ImportError::Sqlx(err)) => {
            tracing::error!(error = %err, "purchases.import");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /api/household/{household_id}/purchases` — list transactions, oldest
/// purchase date first. 403 unless the caller is a member of the household.
async fn list_purchases(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
) -> Result<Json<Vec<TransactionSummary>>, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;
    let transactions = purchase_ledger::list_transactions(&state.pool, &household_id)
        .await
        .map_err(import_error("purchases.list"))?;
    Ok(Json(transactions))
}

/// `GET /api/household/{household_id}/purchases/{transaction_id}` — one
/// transaction with its current lines and its superseded revisions.
///
/// 403 unless the caller is a member; 404 when no such transaction belongs to
/// this household.
async fn get_purchase(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, transaction_id)): Path<(String, String)>,
) -> Result<Json<TransactionDetail>, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;
    let detail = purchase_ledger::get_transaction(&state.pool, &household_id, &transaction_id)
        .await
        .map_err(import_error("purchases.get"))?;
    match detail {
        Some(detail) => Ok(Json(detail)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// `DELETE /api/household/{household_id}/purchases/{transaction_id}` — delete one
/// transaction; its lines and revision history go with it.
///
/// 403 unless the caller is a member; 404 when no such transaction belongs to
/// this household. CSRF is enforced by [`AuthenticatedUser`] (a DELETE).
async fn delete_purchase(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, transaction_id)): Path<(String, String)>,
) -> Result<Json<Value>, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;
    let deleted = purchase_ledger::delete_transaction(&state.pool, &household_id, &transaction_id)
        .await
        .map_err(import_error("purchases.delete"))?;
    if deleted {
        Ok(Json(json!({ "deleted": true })))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Refuse with 403 unless `user_id` is a member of `household_id`.
///
/// A DB error is a logged 500 (fail closed), never treated as "not a member".
async fn require_member(
    state: &AppState,
    user_id: &str,
    household_id: &str,
) -> Result<(), StatusCode> {
    crate::lists::is_member(&state.pool, user_id, household_id)
        .await
        .map_err(db_error("purchases.is_member"))
        .and_then(|member| {
            if member {
                Ok(())
            } else {
                Err(StatusCode::FORBIDDEN)
            }
        })
}

/// Map a purchase-ledger error to a logged 500. Malformed documents and version
/// mismatches are handled with their own bodies in [`import_purchases`], so they
/// never reach this mapper.
fn import_error(context: &'static str) -> impl Fn(ImportError) -> StatusCode {
    move |err| {
        tracing::error!(error = %err, "{context}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// Map a DB error to a logged 500, so a transient failure never fails open.
fn db_error(context: &'static str) -> impl Fn(sqlx::Error) -> StatusCode {
    move |err| {
        tracing::error!(error = %err, "{context}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}
