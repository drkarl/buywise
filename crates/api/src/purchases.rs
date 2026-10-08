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

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use buywise_core::embedding::{self, ConfirmError, ConfirmOutcome, DEFAULT_TOP_K};
use buywise_core::purchase_ledger::{self, ImportError, TransactionDetail, TransactionSummary};
use serde::{Deserialize, Serialize};
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
        // A purchase typed by hand lives under the same collection but on its own
        // path: `POST /purchases` is the import document, so the manual create
        // needs a distinct route rather than a second meaning for the same one.
        .route(
            "/api/household/{household_id}/purchases/manual",
            post(create_manual_purchase),
        )
        .route(
            "/api/household/{household_id}/purchases/{transaction_id}",
            get(get_purchase)
                .put(update_manual_purchase)
                .delete(delete_purchase),
        )
        .route(
            "/api/household/{household_id}/purchases/{transaction_id}/receipt",
            put(set_receipt_link),
        )
        // Embedding-based clarification shortlist for unmatched imported lines
        // (follow-up of the Tesco import #107). A static path, so it takes
        // precedence over `{transaction_id}` exactly like `purchases/manual`.
        .route(
            "/api/household/{household_id}/purchases/unmatched-suggestions",
            get(list_unmatched_suggestions),
        )
        .route(
            "/api/household/{household_id}/purchases/unmatched-suggestions/confirm",
            post(confirm_unmatched_suggestion),
        )
        // Every route here may read a request body (the import and manual
        // documents, and the link body); cap them here so the limit travels with
        // the routes rather than the whole app.
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
        Err(err @ (ImportError::NoSuchTransaction(_) | ImportError::NotManual(_))) => {
            // Neither can arise from an import document; if one ever does, it is
            // a server fault, not a client outcome.
            tracing::error!(error = %err, "purchases.import");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
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

/// Body of `PUT .../purchases/{id}/receipt`: the photo to link, or `null` to
/// clear the link. An absent field is treated as `null`, so a body can clear a
/// link with `{}`.
#[derive(Debug, Deserialize)]
struct SetReceiptLink {
    receipt_image_id: Option<String>,
}

/// `POST /api/household/{household_id}/purchases/manual` — add one purchase by
/// hand.
///
/// 403 unless the caller is a member of `household_id`; 404 if the household
/// does not exist; 422 with `{ "errors": [...] }` when the body is malformed or
/// names a receipt photo that is not the household's (or is expired). On success
/// returns 201 with the stored transaction, its lines and its revisions.
async fn create_manual_purchase(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(status) = require_member(&state, &user.user_id, &household_id).await {
        return status.into_response();
    }
    match purchase_ledger::create_manual_purchase(
        &state.pool,
        &household_id,
        &body,
        (state.clock)(),
    )
    .await
    {
        Ok(detail) => (StatusCode::CREATED, Json(detail)).into_response(),
        Err(err) => ledger_error("purchases.create_manual", err),
    }
}

/// `PUT /api/household/{household_id}/purchases/{transaction_id}` — correct one
/// hand-entered purchase.
///
/// 403 unless the caller is a member; 404 when the household has no such
/// purchase. An imported purchase cannot be edited here (422): it is corrected
/// by re-importing, so the manual endpoint never turns an import into a manual
/// purchase. The edit keeps the transaction's identity, so it corrects the same
/// row — a revision, never a second purchase. Returns the updated transaction
/// with its lines and revisions.
async fn update_manual_purchase(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, transaction_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(status) = require_member(&state, &user.user_id, &household_id).await {
        return status.into_response();
    }
    match purchase_ledger::update_manual_purchase(
        &state.pool,
        &household_id,
        &transaction_id,
        &body,
        (state.clock)(),
    )
    .await
    {
        Ok(detail) => Json(detail).into_response(),
        Err(err) => ledger_error("purchases.update_manual", err),
    }
}

/// `PUT /api/household/{household_id}/purchases/{transaction_id}/receipt` — set
/// or clear the purchase's linked receipt photo.
///
/// 403 unless the caller is a member; 404 when the household has no such
/// purchase; 422 when the named photo is not the household's own unexpired
/// receipt. Linking a photo changes neither the purchase's content nor its
/// revision. Returns the updated transaction.
async fn set_receipt_link(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, transaction_id)): Path<(String, String)>,
    Json(body): Json<SetReceiptLink>,
) -> Response {
    if let Err(status) = require_member(&state, &user.user_id, &household_id).await {
        return status.into_response();
    }
    let now = (state.clock)();
    match purchase_ledger::set_receipt_link(
        &state.pool,
        &household_id,
        &transaction_id,
        body.receipt_image_id.as_deref(),
        now,
    )
    .await
    {
        Ok(()) => {
            match purchase_ledger::get_transaction(&state.pool, &household_id, &transaction_id)
                .await
            {
                Ok(Some(detail)) => Json(detail).into_response(),
                Ok(None) => StatusCode::NOT_FOUND.into_response(),
                Err(err) => ledger_error("purchases.set_receipt_link.read", err),
            }
        }
        Err(err) => ledger_error("purchases.set_receipt_link", err),
    }
}

/// Query parameters for the suggestion list: `?limit=&offset=`.
#[derive(Debug, Deserialize)]
struct SuggestionQuery {
    /// Maximum lines to return (default 50, capped at 500).
    limit: Option<i64>,
    /// Lines to skip (default 0).
    offset: Option<i64>,
}

/// The suggestion list response: the model that produced the scores, whether the
/// embedder is configured, and the lines with their candidates.
#[derive(Debug, Serialize)]
struct UnmatchedSuggestionsResponse {
    /// Model id the scores come from.
    model: String,
    /// Whether an embedder is configured. When `false`, every `candidates` list
    /// is empty — the batch step has not been able to run.
    embedding_enabled: bool,
    /// Unmatched lines, newest purchase first, each with up to five candidates.
    lines: Vec<embedding::LineSuggestion>,
}

/// Body of `POST .../unmatched-suggestions/confirm`.
#[derive(Debug, Deserialize)]
struct ConfirmSuggestionRequest {
    /// `purchase_lines.id` of the line the user chose.
    line_id: String,
    /// `canonical_items.id` the user confirmed for that line.
    canonical_item_id: String,
}

/// `GET /api/household/{household_id}/purchases/unmatched-suggestions` — list
/// unmatched imported lines with their top-5 canonical candidates and scores.
///
/// 403 unless the caller is a member. The candidates are produced by comparing
/// each line's stored embedding against the household's catalogue embeddings;
/// nothing here binds a line. A line whose name has no stored vector (the batch
/// step has not run) is returned with an empty candidate list.
async fn list_unmatched_suggestions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Query(query): Query<SuggestionQuery>,
) -> Result<Json<UnmatchedSuggestionsResponse>, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query.offset.unwrap_or(0).max(0);
    let model = state.embedder.model_id();
    let lines = embedding::unmatched_suggestions(
        &state.pool,
        &household_id,
        &model,
        DEFAULT_TOP_K,
        limit,
        offset,
    )
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "purchases.unmatched_suggestions");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(UnmatchedSuggestionsResponse {
        model,
        embedding_enabled: state.embedder.is_enabled(),
        lines,
    }))
}

/// `POST /api/household/{household_id}/purchases/unmatched-suggestions/confirm` —
/// confirm one line's canonical item.
///
/// 403 unless the caller is a member; 404 when the line or the item is not the
/// household's; 409 when the line is stale or its alias belongs to another item;
/// 422 when the line has no name. On success the line name is
/// learned as an `accepted_match` alias (the shared clarification write path) and
/// every unmatched line of the household that shares the name is bound to the
/// chosen item. Nothing is bound without this call.
async fn confirm_unmatched_suggestion(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Json(body): Json<ConfirmSuggestionRequest>,
) -> Result<Json<ConfirmOutcome>, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;
    match embedding::confirm_suggestion(
        &state.pool,
        &household_id,
        &body.line_id,
        &body.canonical_item_id,
    )
    .await
    {
        Ok(outcome) => Ok(Json(outcome)),
        Err(ConfirmError::NoSuchLine | ConfirmError::NoSuchItem) => Err(StatusCode::NOT_FOUND),
        Err(ConfirmError::UnnamedLine) => Err(StatusCode::UNPROCESSABLE_ENTITY),
        Err(ConfirmError::Conflict) => Err(StatusCode::CONFLICT),
        Err(err @ ConfirmError::Db(_)) => {
            tracing::error!(error = %err, "purchases.unmatched_suggestions.confirm");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map a manual-purchase ledger error to its status and body.
///
/// Expected client outcomes carry their reason in the same `{ "errors": [...] }`
/// shape the import route uses: malformed input and an invalid receipt-photo
/// link are 422, as is an edit of an imported purchase; a missing household or
/// purchase is 404. A database error is a logged 500.
fn ledger_error(context: &'static str, err: ImportError) -> Response {
    match err {
        ImportError::Malformed(errors) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "errors": errors })),
        )
            .into_response(),
        ImportError::NotManual(id) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "errors": [format!(
                "purchase {id} was not entered by hand; imported purchases are corrected by re-importing"
            )] })),
        )
            .into_response(),
        ImportError::NoSuchTransaction(_) | ImportError::NoSuchHousehold(_) => {
            StatusCode::NOT_FOUND.into_response()
        }
        ImportError::UnsupportedFormatVersion(version) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "errors": [format!("unsupported format_version {version}")] })),
        )
            .into_response(),
        ImportError::Sqlx(err) => {
            tracing::error!(error = %err, "{context}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

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
