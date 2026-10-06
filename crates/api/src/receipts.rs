//! Receipt-image endpoints (issue #149, roadmap #114).
//!
//! A household member photographs a receipt after a shop and keeps it privately
//! on the household's own server for 30 days. The bytes live on the server
//! filesystem ([`buywise_core::receipts`]); these routes are the only way to
//! write or read them, and every one of them is authenticated and scoped to the
//! household in the path.
//!
//! | Route | Method | Who | Purpose |
//! | --- | --- | --- | --- |
//! | `/api/household/{id}/receipts` | `GET` | any member | list the household's receipts, newest first |
//! | `/api/household/{id}/receipts` | `POST` | any member | upload one image (raw bytes) |
//! | `/api/household/{id}/receipts/{receipt_id}` | `GET` | any member | the image bytes |
//! | `/api/household/{id}/receipts/{receipt_id}` | `DELETE` | any member | delete the image and its record |
//!
//! # Upload
//!
//! `POST` takes the image as the **raw request body** — not multipart, not JSON —
//! with the per-upload processing choice as a query parameter
//! (`?processing=local`, the default). The client's `Content-Type` and file name
//! are ignored: the type is decided by the file signature
//! ([`buywise_core::receipts::detect_image`]), so a text file renamed to `.jpg`
//! is a 415. The request body is capped at the configured per-image limit
//! ([`buywise_core::receipts::ReceiptConfig::max_image_bytes`]) by a route body
//! limit, so an oversized upload is rejected with 413 before it is buffered
//! whole. Per-household image-count and total-size quotas are enforced on the
//! stored rows (409 when exceeded).
//!
//! `processing=external` is refused with 501 and a message saying external
//! processing does not exist yet: the household owner's decision is that receipt
//! reading is local by default and external processing is only ever a deliberate
//! per-upload choice, and there is no external path in this slice.
//!
//! # Serving
//!
//! `GET .../{receipt_id}` returns the stored bytes with the *detected* content
//! type, `Content-Disposition: inline` with a fixed generated file name,
//! `X-Content-Type-Options: nosniff`, and `Cache-Control: private, no-store` —
//! a private household image is never cached by a shared cache and never sniffed
//! into another type. Every other response from these routes (the JSON list and
//! upload bodies, and the error messages) carries `nosniff` and
//! `private, no-store` too. The bytes are never served from a static path: there is no
//! route that maps a file under the receipt directory.
//!
//! # Authorisation
//!
//! Every handler requires the caller to be a member of the household in the path
//! (`crate::household::require_member`), so a member of another household gets
//! 403 and a receipt id from another household is indistinguishable from a
//! missing one (404). Upload and delete are mutating requests, so the
//! [`AuthenticatedUser`] extractor enforces CSRF on them.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use buywise_core::receipts::{self, ImageKind, ReceiptConfig, ReceiptError, ReceiptImage};

use crate::household::require_member;
use crate::middleware::AuthenticatedUser;
use crate::AppState;

struct ErrorResponse(Box<Response>);

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        *self.0
    }
}

/// Query parameters of an upload. `processing` defaults to `local` when absent.
#[derive(Debug, Default, Deserialize)]
pub struct UploadParams {
    /// Per-upload processing choice; only `local` (the default) is accepted.
    pub processing: Option<String>,
}

/// One receipt image as the app sees it.
///
/// The storage key and the on-disk location are deliberately absent: the app
/// addresses an image by `image_url`, and the file name is an internal detail.
#[derive(Debug, Serialize)]
pub struct ReceiptSummary {
    /// `receipt_images.id`, the handle for the image URL and delete.
    pub id: String,
    /// Upload time (RFC 3339 UTC).
    pub uploaded_at: String,
    /// When the automatic expiry sweep will remove the image.
    pub expires_at: String,
    /// Detected content type of the stored bytes.
    pub content_type: String,
    /// Stored size in bytes.
    pub byte_size: i64,
    /// SHA-256 of the stored bytes, hex.
    pub sha256_hex: String,
    /// `users.id` of the member who uploaded the image, or `None` after account deletion.
    pub uploaded_by: Option<String>,
    /// Processing choice taken at upload; `local` today.
    pub processing: String,
    /// Authenticated path serving the bytes.
    pub image_url: String,
}

impl ReceiptSummary {
    fn from_record(record: ReceiptImage) -> Self {
        let image_url = format!(
            "/api/household/{}/receipts/{}",
            record.household_id, record.id
        );
        Self {
            id: record.id,
            uploaded_at: record.uploaded_at,
            expires_at: record.expires_at,
            content_type: record.content_type,
            byte_size: record.byte_size,
            sha256_hex: record.sha256_hex,
            uploaded_by: record.uploaded_by,
            processing: record.processing,
            image_url,
        }
    }
}

/// Build the receipt routes as a `Router<AppState>`, merged by
/// [`crate::api_router`]. Paths are absolute, like the sibling modules.
///
/// `config` supplies the per-image body limit, which the router needs at build
/// time to cap the upload body (the other quota limits are checked against the
/// stored rows by [`buywise_core::receipts::store_receipt`]).
pub fn router(config: &ReceiptConfig) -> Router<AppState> {
    let body_limit = DefaultBodyLimit::max(config.max_image_bytes as usize);
    Router::new()
        .route(
            "/api/household/{household_id}/receipts",
            get(list_receipts).post(upload_receipt).layer(body_limit),
        )
        .route(
            "/api/household/{household_id}/receipts/{receipt_id}",
            get(download_receipt).delete(delete_receipt),
        )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/household/{household_id}/receipts` — the household's receipts,
/// newest first.
///
/// 403 unless the caller is a member of `household_id`.
async fn list_receipts(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
) -> Result<Response, StatusCode> {
    require_member(&state, &user.user_id, &household_id).await?;
    let records = receipts::list_receipts(&state.pool, &household_id, (state.clock)())
        .await
        .map_err(receipt_error("receipts.list"))?;
    let summaries: Vec<ReceiptSummary> = records
        .into_iter()
        .map(ReceiptSummary::from_record)
        .collect();
    Ok(private_json(StatusCode::OK, Json(summaries)))
}

/// `POST /api/household/{household_id}/receipts` — store one uploaded image.
///
/// The body is the raw image; see the module docs for the accepted types, the
/// limits and the processing choice. 201 with the stored receipt on success.
async fn upload_receipt(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(household_id): Path<String>,
    Query(params): Query<UploadParams>,
    body: Bytes,
) -> Result<Response, ErrorResponse> {
    require_member(&state, &user.user_id, &household_id)
        .await
        .map_err(|status| ErrorResponse(Box::new(status.into_response())))?;

    let processing = params
        .processing
        .as_deref()
        .unwrap_or(receipts::PROCESSING_LOCAL);

    let record = receipts::store_receipt(
        &state.pool,
        &state.receipt_config,
        &household_id,
        &user.user_id,
        processing,
        &body,
        (state.clock)(),
    )
    .await
    .map_err(upload_error)?;

    Ok(private_json(
        StatusCode::CREATED,
        Json(ReceiptSummary::from_record(record)),
    ))
}

/// `GET /api/household/{household_id}/receipts/{receipt_id}` — the image bytes.
///
/// 403 unless the caller is a member of `household_id`; 404 when the household
/// has no such receipt (including a receipt of another household) or its file is
/// no longer on disk. The response always carries the stored content type, an
/// inline disposition with a fixed generated file name, `nosniff` and
/// `Cache-Control: private, no-store`.
async fn download_receipt(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, receipt_id)): Path<(String, String)>,
) -> Result<Response, ErrorResponse> {
    require_member(&state, &user.user_id, &household_id)
        .await
        .map_err(|status| ErrorResponse(Box::new(status.into_response())))?;

    let Some(record) =
        receipts::get_receipt(&state.pool, &household_id, &receipt_id, (state.clock)())
            .await
            .map_err(|err| {
                ErrorResponse(Box::new(internal_error("receipts.download_lookup", err)))
            })?
    else {
        return Err(ErrorResponse(Box::new(
            StatusCode::NOT_FOUND.into_response(),
        )));
    };

    let bytes = match receipts::read_file(&state.receipt_config.dir, &record.storage_key) {
        Ok(bytes) => bytes,
        // The record survived but its bytes are gone (a hand-removed file, or a
        // restore that did not carry the images): the image is simply not
        // available. The expiry sweep will eventually remove the record.
        Err(ReceiptError::FileMissing { .. }) => {
            tracing::error!(receipt = %record.id, "receipts.download: file missing");
            return Err(ErrorResponse(Box::new(
                StatusCode::NOT_FOUND.into_response(),
            )));
        }
        Err(err) => {
            return Err(ErrorResponse(Box::new(internal_error(
                "receipts.download_read",
                err,
            ))))
        }
    };

    let mut response = Response::new(axum::body::Body::from(bytes));
    let headers = response.headers_mut();
    // The stored, signature-checked type — never a client-supplied one.
    match HeaderValue::from_str(&record.content_type) {
        Ok(value) => {
            headers.insert(header::CONTENT_TYPE, value);
        }
        Err(err) => {
            tracing::error!(error = %err, receipt = %record.id, "receipts.download: bad content type");
            return Err(ErrorResponse(Box::new(
                StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            )));
        }
    }
    let extension = ImageKind::from_content_type(&record.content_type)
        .map(ImageKind::extension)
        .unwrap_or("bin");
    let filename = format!("receipt-{}.{extension}", safe_receipt_id(&record.id));
    match HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
        Ok(value) => {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        Err(err) => {
            tracing::error!(error = %err, receipt = %record.id, "receipts.download: bad disposition");
            return Err(ErrorResponse(Box::new(
                StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            )));
        }
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // A private image is never cached by a shared cache, and never stored on
    // disk by the browser: every view re-authenticates.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

/// `DELETE /api/household/{household_id}/receipts/{receipt_id}` — remove the
/// image and its record.
///
/// 403 unless the caller is a member of `household_id`; 404 when the household
/// has no such receipt. CSRF is enforced by [`AuthenticatedUser`] (a DELETE).
async fn delete_receipt(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((household_id, receipt_id)): Path<(String, String)>,
) -> Result<StatusCode, ErrorResponse> {
    require_member(&state, &user.user_id, &household_id)
        .await
        .map_err(|status| ErrorResponse(Box::new(status.into_response())))?;

    let removed = receipts::delete_receipt(
        &state.pool,
        &state.receipt_config,
        &household_id,
        &receipt_id,
    )
    .await
    .map_err(|err| ErrorResponse(Box::new(internal_error("receipts.delete", err))))?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ErrorResponse(Box::new(
            StatusCode::NOT_FOUND.into_response(),
        )))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map an upload failure to its status and a message the app can show.
///
/// These are all expected client outcomes, so each carries the reason: an
/// oversized image (413), a signature that is not an accepted format (415), a
/// quota the household has reached (409), an unknown processing value (422) and
/// external processing that does not exist yet (501).
fn upload_error(err: ReceiptError) -> ErrorResponse {
    let (status, message) = match &err {
        ReceiptError::ImageTooLarge { .. } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "The photo is larger than this household's per-image limit.".to_string(),
        ),
        ReceiptError::UnsupportedImageType => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Only JPEG, PNG, WebP or HEIC photos are accepted.".to_string(),
        ),
        ReceiptError::TooManyImages { .. } => (
            StatusCode::CONFLICT,
            "This household keeps as many receipts as it is allowed; delete some first."
                .to_string(),
        ),
        ReceiptError::HouseholdStorageFull { .. } => (
            StatusCode::CONFLICT,
            "This household's receipt storage is full; delete some receipts first.".to_string(),
        ),
        ReceiptError::ExternalProcessingUnsupported => (
            StatusCode::NOT_IMPLEMENTED,
            "External receipt processing is not available yet; only local processing is accepted."
                .to_string(),
        ),
        ReceiptError::UnknownProcessing(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Unknown processing choice; only \"local\" is accepted.".to_string(),
        ),
        _ => {
            tracing::error!(error = %err, "receipts.upload");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The receipt could not be stored.".to_string(),
            )
        }
    };
    ErrorResponse(Box::new(error_response(status, &message)))
}

/// Map a storage error to a logged 500 with a plain message.
fn internal_error(context: &'static str, err: ReceiptError) -> Response {
    tracing::error!(error = %err, "{context}");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "The receipt request could not be completed.",
    )
}

/// Map a query error to a logged 500.
fn receipt_error(context: &'static str) -> impl Fn(ReceiptError) -> StatusCode {
    move |err| {
        tracing::error!(error = %err, "{context}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// A JSON response carrying this module's private-response headers:
/// `X-Content-Type-Options: nosniff` (nothing here is sniffed into another type)
/// and `Cache-Control: private, no-store` (a household's receipt list and
/// messages are as private as the images themselves).
fn private_json<T: serde::Serialize>(status: StatusCode, body: Json<T>) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// A JSON error body, so a refusal explains itself instead of returning a bare
/// status.
fn error_response(status: StatusCode, message: &str) -> Response {
    private_json(status, Json(serde_json::json!({ "error": message })))
}

/// Restrict a receipt id to characters safe in a `Content-Disposition` file
/// name. The id is a generated UUID, so this only guards against an unusual
/// stored value.
fn safe_receipt_id(receipt_id: &str) -> String {
    receipt_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}
