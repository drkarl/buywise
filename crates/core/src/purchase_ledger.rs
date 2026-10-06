//! Purchase ledger core (issue #148, roadmap #114).
//!
//! One household-scoped model for what the household actually bought, fed by
//! structured imports today and receipt reading later. The schema is migration
//! `019_purchase_ledger.sql`; the import format is documented in
//! `docs/purchase-ledger.md`. This module is the one implementation shared by
//! the CLI (`buywise-cli import-purchases`) and the API
//! (`crates/api/src/purchases.rs`), so the two cannot drift.
//!
//! # What a ledger holds
//!
//! A `purchase_transactions` row is one purchase at one retailer on one date:
//! its total, currency, source identity and the original source payload.
//! Its `purchase_lines` rows carry the original text plus whatever the source
//! knew (name, brand, quantity, size, unit price, line total, discount,
//! deposit, optional retailer-product and canonical-item links). Every line is
//! kept, and `match_status` defaults to `unmatched` without being inferred from
//! the optional links.
//!
//! # Identity and idempotency
//!
//! Every imported transaction has a `transaction_identity`: the caller's
//! `source_transaction_id` when supplied, otherwise the computed [`fingerprint`]
//! of retailer + purchase date + total + the ordered line texts. The unique key
//! `(household_id, retailer, source_kind, transaction_identity)` is what makes a
//! re-import idempotent: the same identity at the same retailer upserts the one
//! row instead of duplicating it, so totals are never summed over duplicate links.
//!
//! * An import whose content is byte-for-byte the stored content is a no-op
//!   that reports [`ImportStatus::Unchanged`].
//! * An import of the same identity with *different* content is a correction:
//!   the previous revision is snapshotted into `purchase_transaction_revisions`
//!   (so an audit reader keeps the earlier version), the live row's `revision`
//!   is bumped, and the result reports [`ImportStatus::Updated`].
//!
//! Under a computed fingerprint the identity *is* retailer + date + total + line
//! texts, so changing one of those is a different identity, not a correction.
//! Callers that must correct those fields supply `source_transaction_id`; the
//! format document states this explicitly.
//!
//! # Scope
//!
//! The functions here expect a database the caller has already migrated (the
//! API migrates at startup; the CLI's `import-purchases` also migrates) and
//! never run migrations themselves. Every read and write filters on the
//! household id, so one household can never see or change another's ledger.

use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqliteConnection, SqlitePool};

use crate::db;

/// The only import layout this build reads. Bumping it accompanies an
/// incompatible change to the format documented in `docs/purchase-ledger.md`.
pub const IMPORT_FORMAT_VERSION: i64 = 1;

/// Currency applied when an imported transaction omits one. Money is integer
/// cents; the ISO-4217 code is separate.
pub const DEFAULT_CURRENCY: &str = "EUR";

/// The `source_kind` imports write (`receipt` is reserved for receipt reading).
pub const SOURCE_KIND_IMPORT: &str = "import";

/// Unmatched status value for a purchase line.
pub const MATCH_UNMATCHED: &str = "unmatched";
/// Matched status value for a purchase line.
pub const MATCH_MATCHED: &str = "matched";

/// Maximum transactions accepted in one import document.
pub const MAX_TRANSACTIONS: usize = 1000;
/// Maximum lines accepted on one imported transaction.
pub const MAX_LINES_PER_TRANSACTION: usize = 1000;

/// Version tag prefixed to every fingerprint, so a future identity-rule change
/// can be told apart from a fingerprint computed today.
const FINGERPRINT_PREFIX: &str = "pl1:";

/// Errors produced by the purchase-ledger operations.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The document declared a `format_version` this build does not read.
    #[error("unsupported purchase-import format_version {0}; this build reads version {IMPORT_FORMAT_VERSION}")]
    UnsupportedFormatVersion(i64),

    /// The document is malformed. Each entry names the offending field path and
    /// what was wrong, e.g. `transactions[0].lines[1].line_total_cents: expected
    /// an integer`.
    #[error("malformed purchase import: {}", .0.join("; "))]
    Malformed(Vec<String>),

    /// The target household does not exist.
    #[error("no household with id {0}")]
    NoSuchHousehold(String),

    /// A query or transaction failed at the sqlx layer.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

/// One transaction after parsing and validation, before it is written.
///
/// `source_payload` is the original source payload as compact JSON text: the
/// document's own `source_payload` when supplied, otherwise the transaction
/// object exactly as it appeared in the import.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParsedTransaction {
    /// The retailer's own transaction id, when the source has one.
    pub source_transaction_id: Option<String>,
    /// Retailer token, trimmed and lowercased.
    pub retailer: String,
    /// Calendar date of the purchase (`YYYY-MM-DD`).
    pub purchase_date: String,
    /// ISO-4217 currency code (uppercase), defaulting to [`DEFAULT_CURRENCY`].
    pub currency: String,
    /// Transaction total in integer cents.
    pub total_cents: i64,
    /// Original source payload (JSON text, stored verbatim).
    pub source_payload: String,
    /// The transaction's lines, in ordinal order.
    pub lines: Vec<ParsedLine>,
}

/// One parsed, validated purchase line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParsedLine {
    /// The line text exactly as the source printed it.
    pub original_text: String,
    /// Product name, when the source separated one from the text.
    pub name: Option<String>,
    /// Brand, when known.
    pub brand: Option<String>,
    /// Quantity bought (decimal for weight/volume), when known.
    pub quantity: Option<f64>,
    /// Pack size value, when known.
    pub size_value: Option<f64>,
    /// Pack size unit (e.g. `l`, `g`), when known.
    pub size_unit: Option<String>,
    /// Unit price in integer cents, when known.
    pub unit_price_cents: Option<i64>,
    /// Line total in integer cents.
    pub line_total_cents: i64,
    /// Discount applied to the line, in integer cents (0 when absent).
    pub line_discount_cents: i64,
    /// Deposit/recycling charge on the line, in integer cents (0 when absent).
    pub deposit_cents: i64,
    /// Optional link to a shared `retailer_products` row.
    pub retailer_product_id: Option<String>,
    /// Optional link to the household's `canonical_items` row.
    pub canonical_item_id: Option<String>,
    /// `unmatched` or `matched`.
    pub match_status: String,
}

/// Outcome of one imported transaction.
#[derive(Debug, Clone, Serialize)]
pub struct TransactionResult {
    /// `purchase_transactions.id` the import wrote to.
    pub transaction_id: String,
    /// The revision after the import (1 for a first insert).
    pub revision: i64,
    /// Whether the transaction was inserted, corrected, or left unchanged.
    pub status: ImportStatus,
}

/// What happened to one transaction on import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportStatus {
    /// A new transaction and its lines were written.
    Inserted,
    /// The transaction existed with different content; its previous revision was
    /// snapshotted and the live row updated.
    Updated,
    /// The transaction existed with identical content; nothing was written.
    Unchanged,
}

/// Result of importing a document.
#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    /// Per-transaction outcome, in document order.
    pub results: Vec<TransactionResult>,
    /// Number of transactions inserted.
    pub inserted: usize,
    /// Number of transactions corrected (revision bumped).
    pub updated: usize,
    /// Number of transactions left unchanged.
    pub unchanged: usize,
}

/// One transaction as listed (no lines).
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TransactionSummary {
    /// `purchase_transactions.id`.
    pub id: String,
    /// Owning household.
    pub household_id: String,
    /// Retailer token.
    pub retailer: String,
    /// Purchase date (`YYYY-MM-DD`).
    pub purchase_date: String,
    /// `import` (today) or `receipt` (reserved).
    pub source_kind: String,
    /// The retailer's own id, when supplied.
    pub source_transaction_id: Option<String>,
    /// `source_id` or `fingerprint`.
    pub identity_kind: String,
    /// The computed content fingerprint (always present).
    pub fingerprint: String,
    /// ISO-4217 currency code.
    pub currency: String,
    /// Total in integer cents.
    pub total_cents: i64,
    /// Current revision; 1 until a correction.
    pub revision: i64,
    /// First write time (RFC 3339 UTC).
    pub created_at: String,
    /// Last write time (RFC 3339 UTC).
    pub updated_at: String,
    /// Number of lines on the current revision.
    pub line_count: i64,
}

/// One stored line of a transaction.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct LineView {
    /// `purchase_lines.id`.
    pub id: String,
    /// Owning transaction.
    pub transaction_id: String,
    /// Position of the line within the transaction (0-based).
    pub line_ordinal: i64,
    /// Line text as printed by the source.
    pub original_text: String,
    /// Product name, when known.
    pub name: Option<String>,
    /// Brand, when known.
    pub brand: Option<String>,
    /// Quantity bought.
    pub quantity: Option<f64>,
    /// Pack size value.
    pub size_value: Option<f64>,
    /// Pack size unit.
    pub size_unit: Option<String>,
    /// Unit price in integer cents.
    pub unit_price_cents: Option<i64>,
    /// Line total in integer cents.
    pub line_total_cents: i64,
    /// Line discount in integer cents.
    pub line_discount_cents: i64,
    /// Deposit in integer cents.
    pub deposit_cents: i64,
    /// Optional retailer-product link.
    pub retailer_product_id: Option<String>,
    /// Optional canonical-item link.
    pub canonical_item_id: Option<String>,
    /// `unmatched` or `matched`.
    pub match_status: String,
    /// Revision this line belongs to.
    pub revision: i64,
    /// Write time (RFC 3339 UTC).
    pub created_at: String,
}

/// One superseded revision of a transaction.
#[derive(Debug, Clone, Serialize)]
pub struct RevisionView {
    /// The revision number that was superseded.
    pub revision: i64,
    /// Retailer at that revision.
    pub retailer: String,
    /// Purchase date at that revision.
    pub purchase_date: String,
    /// Currency at that revision.
    pub currency: String,
    /// Total in integer cents at that revision.
    pub total_cents: i64,
    /// Source payload at that revision.
    pub source_payload: String,
    /// When the revision was superseded (RFC 3339 UTC).
    pub superseded_at: String,
    /// The lines at that revision, as stored JSON.
    pub lines: Value,
}

/// A transaction with its current lines and its superseded revisions.
#[derive(Debug, Clone, Serialize)]
pub struct TransactionDetail {
    /// Header fields of the live transaction.
    #[serde(flatten)]
    pub transaction: TransactionSummary,
    /// Current lines, in ordinal order.
    pub lines: Vec<LineView>,
    /// Superseded revisions, oldest first.
    pub revisions: Vec<RevisionView>,
}

/// The stable content fingerprint: `pl1:<sha256 hex>` over the newline-joined
/// UTF-8 of the retailer (already normalised), the purchase date, the total in
/// cents, and the ordered line texts.
///
/// The exact inputs are documented in `docs/purchase-ledger.md`, because an
/// importer must be able to compute the same value to be idempotent across
/// runs. The `pl1:` prefix versions the rule.
pub fn fingerprint(
    retailer: &str,
    purchase_date: &str,
    total_cents: i64,
    line_texts: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(retailer.as_bytes());
    hasher.update(b"\n");
    hasher.update(purchase_date.as_bytes());
    hasher.update(b"\n");
    hasher.update(total_cents.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(line_texts.join("\n").as_bytes());
    format!("{FINGERPRINT_PREFIX}{}", hex::encode(hasher.finalize()))
}

impl ParsedTransaction {
    /// This transaction's fingerprint, computed from its normalised fields.
    pub fn fingerprint(&self) -> String {
        let texts: Vec<String> = self.lines.iter().map(|l| l.original_text.clone()).collect();
        fingerprint(
            &self.retailer,
            &self.purchase_date,
            self.total_cents,
            &texts,
        )
    }

    /// The identity key and which rule produced it.
    fn identity(&self) -> (&str, &'static str) {
        match &self.source_transaction_id {
            Some(id) => (id.as_str(), "source_id"),
            None => ("", "fingerprint"),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing and validation
// ---------------------------------------------------------------------------

/// Parse and validate an import document.
///
/// Structural and semantic problems are reported together as
/// [`ImportError::Malformed`], each entry naming the field path, so a caller can
/// fix a whole document in one pass rather than one field per attempt. A
/// `format_version` this build does not read is [`ImportError::UnsupportedFormatVersion`].
pub fn parse_import_document(document: &Value) -> Result<Vec<ParsedTransaction>, ImportError> {
    let mut errors: Vec<String> = Vec::new();

    let Some(root) = root_object(document, &mut errors) else {
        return Err(ImportError::Malformed(errors));
    };
    reject_unknown_keys(root, "$", &["format_version", "transactions"], &mut errors);

    if !errors.is_empty() {
        return Err(ImportError::Malformed(errors));
    }

    match root.get("format_version") {
        None => {
            return Err(ImportError::Malformed(vec![
                "$.format_version: missing required field".to_string(),
            ]))
        }
        Some(value) => match value.as_i64() {
            Some(version) if version == IMPORT_FORMAT_VERSION => {}
            Some(other) => return Err(ImportError::UnsupportedFormatVersion(other)),
            None => {
                return Err(ImportError::Malformed(vec![
                    "$.format_version: expected an integer".to_string(),
                ]))
            }
        },
    }

    let transactions = match root.get("transactions") {
        None => {
            return Err(ImportError::Malformed(vec![
                "$.transactions: missing required field".to_string(),
            ]))
        }
        Some(value) => match value.as_array() {
            Some(array) => array,
            None => {
                return Err(ImportError::Malformed(vec![
                    "$.transactions: expected an array".to_string(),
                ]))
            }
        },
    };

    if transactions.is_empty() {
        return Err(ImportError::Malformed(vec![
            "$.transactions: must contain at least one transaction".to_string(),
        ]));
    }
    if transactions.len() > MAX_TRANSACTIONS {
        return Err(ImportError::Malformed(vec![format!(
            "$.transactions: at most {MAX_TRANSACTIONS} transactions per import"
        )]));
    }

    let mut parsed = Vec::with_capacity(transactions.len());
    for (index, transaction) in transactions.iter().enumerate() {
        let path = format!("transactions[{index}]");
        if let Some(parsed_transaction) = parse_transaction(transaction, &path, &mut errors) {
            parsed.push(parsed_transaction);
        }
    }

    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(ImportError::Malformed(errors))
    }
}

fn root_object<'a>(
    document: &'a Value,
    errors: &mut Vec<String>,
) -> Option<&'a Map<String, Value>> {
    match document.as_object() {
        Some(map) => Some(map),
        None => {
            errors.push("$: expected a JSON object".to_string());
            None
        }
    }
}

fn parse_transaction(
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<ParsedTransaction> {
    let Some(map) = value.as_object() else {
        errors.push(format!("{path}: expected an object"));
        return None;
    };
    reject_unknown_keys(
        map,
        path,
        &[
            "source_transaction_id",
            "retailer",
            "purchase_date",
            "currency",
            "total_cents",
            "source_payload",
            "lines",
        ],
        errors,
    );

    let source_transaction_id = opt_string(map, path, "source_transaction_id", errors);
    let retailer = req_string(map, path, "retailer", errors).map(|s| s.to_lowercase());
    let purchase_date = req_date(map, path, "purchase_date", errors);
    let currency = opt_string(map, path, "currency", errors)
        .map(|s| s.to_uppercase())
        .unwrap_or_else(|| DEFAULT_CURRENCY.to_string());
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_uppercase()) {
        errors.push(format!(
            "{path}.currency: expected a three-letter ISO-4217 code"
        ));
    }
    let total_cents = req_i64(map, path, "total_cents", errors);

    let source_payload = match map.get("source_payload") {
        Some(payload) if !payload.is_null() => compact_json(payload),
        _ => compact_json(value),
    };

    let lines = match map.get("lines") {
        None => {
            errors.push(format!("{path}.lines: missing required field"));
            Vec::new()
        }
        Some(Value::Array(array)) => {
            if array.len() > MAX_LINES_PER_TRANSACTION {
                errors.push(format!(
                    "{path}.lines: at most {MAX_LINES_PER_TRANSACTION} lines per transaction"
                ));
            }
            array
                .iter()
                .enumerate()
                .filter_map(|(index, line)| {
                    parse_line(line, &format!("{path}.lines[{index}]"), errors)
                })
                .collect()
        }
        Some(_) => {
            errors.push(format!("{path}.lines: expected an array"));
            Vec::new()
        }
    };

    Some(ParsedTransaction {
        source_transaction_id,
        retailer: retailer?,
        purchase_date: purchase_date?,
        currency,
        total_cents: total_cents?,
        source_payload,
        lines,
    })
}

fn parse_line(value: &Value, path: &str, errors: &mut Vec<String>) -> Option<ParsedLine> {
    let Some(map) = value.as_object() else {
        errors.push(format!("{path}: expected an object"));
        return None;
    };
    reject_unknown_keys(
        map,
        path,
        &[
            "original_text",
            "name",
            "brand",
            "quantity",
            "size_value",
            "size_unit",
            "unit_price_cents",
            "line_total_cents",
            "line_discount_cents",
            "deposit_cents",
            "retailer_product_id",
            "canonical_item_id",
            "match_status",
        ],
        errors,
    );

    let original_text = req_string(map, path, "original_text", errors);
    let name = opt_string(map, path, "name", errors);
    let brand = opt_string(map, path, "brand", errors);
    let quantity = opt_positive_f64(map, path, "quantity", errors);
    let size_value = opt_f64(map, path, "size_value", errors);
    let size_unit = opt_string(map, path, "size_unit", errors);
    let unit_price_cents = opt_i64(map, path, "unit_price_cents", errors);
    let line_total_cents = req_i64(map, path, "line_total_cents", errors);
    let line_discount_cents = opt_i64(map, path, "line_discount_cents", errors).unwrap_or(0);
    let deposit_cents = opt_i64(map, path, "deposit_cents", errors).unwrap_or(0);
    if line_discount_cents < 0 {
        errors.push(format!("{path}.line_discount_cents: must not be negative"));
    }
    if deposit_cents < 0 {
        errors.push(format!("{path}.deposit_cents: must not be negative"));
    }
    let retailer_product_id = opt_string(map, path, "retailer_product_id", errors);
    let canonical_item_id = opt_string(map, path, "canonical_item_id", errors);
    let match_status = opt_string(map, path, "match_status", errors)
        .unwrap_or_else(|| MATCH_UNMATCHED.to_string());
    if match_status != MATCH_UNMATCHED && match_status != MATCH_MATCHED {
        errors.push(format!(
            "{path}.match_status: expected {MATCH_UNMATCHED:?} or {MATCH_MATCHED:?}"
        ));
    }

    Some(ParsedLine {
        original_text: original_text?,
        name,
        brand,
        quantity,
        size_value,
        size_unit,
        unit_price_cents,
        line_total_cents: line_total_cents?,
        line_discount_cents,
        deposit_cents,
        retailer_product_id,
        canonical_item_id,
        match_status,
    })
}

fn reject_unknown_keys(
    map: &Map<String, Value>,
    path: &str,
    allowed: &[&str],
    errors: &mut Vec<String>,
) {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            errors.push(format!("{path}.{key}: unknown field"));
        }
    }
}

/// A required non-empty string.
fn req_string(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    match map.get(key) {
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            None
        }
        Some(value) => match value.as_str() {
            Some(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Some(_) => {
                errors.push(format!("{path}.{key}: must not be empty"));
                None
            }
            None => {
                errors.push(format!("{path}.{key}: expected a string"));
                None
            }
        },
    }
}

/// An optional string; absent, `null` and whitespace-only all become `None`.
fn opt_string(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_str() {
            Some(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            Some(_) => None,
            None => {
                errors.push(format!("{path}.{key}: expected a string"));
                None
            }
        },
    }
}

/// A required `YYYY-MM-DD` calendar date, returned in canonical form.
fn req_date(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    let raw = match map.get(key) {
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            return None;
        }
        Some(Value::String(text)) => text.clone(),
        Some(_) => {
            errors.push(format!("{path}.{key}: expected a YYYY-MM-DD string"));
            return None;
        }
    };
    match chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d") {
        Ok(date) => Some(date.format("%Y-%m-%d").to_string()),
        Err(_) => {
            errors.push(format!("{path}.{key}: expected a YYYY-MM-DD date"));
            None
        }
    }
}

/// A required integer (a JSON float such as `1.0` is rejected, not truncated).
fn req_i64(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<i64> {
    match map.get(key) {
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            None
        }
        Some(value) => match value.as_i64() {
            Some(number) => Some(number),
            None => {
                errors.push(format!("{path}.{key}: expected an integer"));
                None
            }
        },
    }
}

/// An optional integer.
fn opt_i64(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<i64> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_i64() {
            Some(number) => Some(number),
            None => {
                errors.push(format!("{path}.{key}: expected an integer"));
                None
            }
        },
    }
}

/// An optional non-negative number.
fn opt_f64(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<f64> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_f64() {
            Some(number) if number >= 0.0 => Some(number),
            Some(_) => {
                errors.push(format!("{path}.{key}: must not be negative"));
                None
            }
            None => {
                errors.push(format!("{path}.{key}: expected a number"));
                None
            }
        },
    }
}

/// An optional strictly-positive number (`quantity`).
fn opt_positive_f64(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<f64> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_f64() {
            Some(number) if number > 0.0 => Some(number),
            Some(_) => {
                errors.push(format!("{path}.{key}: must be greater than zero"));
                None
            }
            None => {
                errors.push(format!("{path}.{key}: expected a number"));
                None
            }
        },
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Import one document into `household_id`.
///
/// The whole document is one transaction: either every parsed transaction is
/// written (or found unchanged) or none is. Errors with
/// [`ImportError::NoSuchHousehold`] when the household does not exist, so an
/// import can never create an orphan ledger.
pub async fn import_transactions(
    pool: &SqlitePool,
    household_id: &str,
    document: &Value,
) -> Result<ImportOutcome, ImportError> {
    let transactions = parse_import_document(document)?;

    let exists: Option<String> = sqlx::query_scalar("SELECT id FROM households WHERE id = ?1")
        .bind(household_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Err(ImportError::NoSuchHousehold(household_id.to_string()));
    }

    validate_line_links(pool, household_id, &transactions).await?;
    let mut conn = pool.begin().await?;
    let mut results = Vec::with_capacity(transactions.len());
    for transaction in &transactions {
        results.push(import_one(&mut conn, household_id, transaction).await?);
    }
    conn.commit().await?;

    let mut outcome = ImportOutcome {
        results,
        inserted: 0,
        updated: 0,
        unchanged: 0,
    };
    for result in &outcome.results {
        match result.status {
            ImportStatus::Inserted => outcome.inserted += 1,
            ImportStatus::Updated => outcome.updated += 1,
            ImportStatus::Unchanged => outcome.unchanged += 1,
        }
    }
    Ok(outcome)
}

async fn validate_line_links(
    pool: &SqlitePool,
    household_id: &str,
    transactions: &[ParsedTransaction],
) -> Result<(), ImportError> {
    let mut errors = Vec::new();
    for (transaction_index, transaction) in transactions.iter().enumerate() {
        for (line_index, line) in transaction.lines.iter().enumerate() {
            let path = format!("transactions[{transaction_index}].lines[{line_index}]");
            if let Some(id) = &line.retailer_product_id {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM retailer_products WHERE id = ?1)",
                )
                .bind(id)
                .fetch_one(pool)
                .await?;
                if !exists {
                    errors.push(format!(
                        "{path}.retailer_product_id: no such retailer product"
                    ));
                }
            }
            if let Some(id) = &line.canonical_item_id {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM canonical_items WHERE id = ?1 AND household_id = ?2)",
                )
                .bind(id)
                .bind(household_id)
                .fetch_one(pool)
                .await?;
                if !exists {
                    errors.push(format!(
                        "{path}.canonical_item_id: no such canonical item in this household"
                    ));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ImportError::Malformed(errors))
    }
}

/// A stored transaction header, read back for comparison and revision snapshots.
#[derive(Debug, Clone, FromRow)]
struct StoredTransaction {
    retailer: String,
    purchase_date: String,
    currency: String,
    total_cents: i64,
    revision: i64,
    source_payload: String,
}

/// A stored line, read back for comparison and revision snapshots. Field names
/// and types match [`ParsedLine`] so both serialise to the same JSON content.
#[derive(Debug, Clone, Serialize, FromRow)]
struct LineRow {
    original_text: String,
    name: Option<String>,
    brand: Option<String>,
    quantity: Option<f64>,
    size_value: Option<f64>,
    size_unit: Option<String>,
    unit_price_cents: Option<i64>,
    line_total_cents: i64,
    line_discount_cents: i64,
    deposit_cents: i64,
    retailer_product_id: Option<String>,
    canonical_item_id: Option<String>,
    match_status: String,
}

async fn import_one(
    conn: &mut SqliteConnection,
    household_id: &str,
    transaction: &ParsedTransaction,
) -> Result<TransactionResult, ImportError> {
    let fingerprint = transaction.fingerprint();
    let (source_id, identity_kind) = transaction.identity();
    let identity = if source_id.is_empty() {
        fingerprint.as_str()
    } else {
        source_id
    };

    let now = db::now_iso();
    let id = uuid::Uuid::new_v4().to_string();
    let inserted = insert_transaction(
        conn,
        &id,
        household_id,
        transaction,
        identity,
        identity_kind,
        &fingerprint,
        1,
        &now,
    )
    .await?;
    if inserted {
        insert_lines(conn, &id, &transaction.lines, 1, &now).await?;
        return Ok(TransactionResult {
            transaction_id: id,
            revision: 1,
            status: ImportStatus::Inserted,
        });
    }

    let target_id: String = sqlx::query_scalar(
        "SELECT id FROM purchase_transactions \
         WHERE household_id = ?1 AND retailer = ?2 AND source_kind = ?3 \
         AND transaction_identity = ?4",
    )
    .bind(household_id)
    .bind(&transaction.retailer)
    .bind(SOURCE_KIND_IMPORT)
    .bind(identity)
    .fetch_one(&mut *conn)
    .await?;

    let stored: StoredTransaction = sqlx::query_as(
        "SELECT retailer, purchase_date, currency, total_cents, revision, source_payload \
         FROM purchase_transactions WHERE id = ?1",
    )
    .bind(&target_id)
    .fetch_one(&mut *conn)
    .await?;
    let stored_lines = fetch_line_rows(conn, &target_id).await?;

    if stored_content(&stored, &stored_lines) == parsed_content(transaction) {
        return Ok(TransactionResult {
            transaction_id: target_id,
            revision: stored.revision,
            status: ImportStatus::Unchanged,
        });
    }

    let next_revision = stored.revision + 1;
    let lines_json = serde_json::to_string(&stored_lines).unwrap_or_else(|_| "[]".to_string());
    sqlx::query(
        "INSERT INTO purchase_transaction_revisions \
         (id, transaction_id, revision, retailer, purchase_date, currency, total_cents, \
          source_payload, lines_json, superseded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&target_id)
    .bind(stored.revision)
    .bind(&stored.retailer)
    .bind(&stored.purchase_date)
    .bind(&stored.currency)
    .bind(stored.total_cents)
    .bind(&stored.source_payload)
    .bind(&lines_json)
    .bind(&now)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "UPDATE purchase_transactions SET retailer = ?1, purchase_date = ?2, currency = ?3, \
         total_cents = ?4, source_transaction_id = ?5, transaction_identity = ?6, \
         identity_kind = ?7, fingerprint = ?8, source_payload = ?9, revision = ?10, \
         updated_at = ?11 WHERE id = ?12",
    )
    .bind(&transaction.retailer)
    .bind(&transaction.purchase_date)
    .bind(&transaction.currency)
    .bind(transaction.total_cents)
    .bind(&transaction.source_transaction_id)
    .bind(identity)
    .bind(identity_kind)
    .bind(&fingerprint)
    .bind(&transaction.source_payload)
    .bind(next_revision)
    .bind(&now)
    .bind(&target_id)
    .execute(&mut *conn)
    .await?;

    sqlx::query("DELETE FROM purchase_lines WHERE transaction_id = ?1")
        .bind(&target_id)
        .execute(&mut *conn)
        .await?;
    insert_lines(conn, &target_id, &transaction.lines, next_revision, &now).await?;

    Ok(TransactionResult {
        transaction_id: target_id,
        revision: next_revision,
        status: ImportStatus::Updated,
    })
}

#[allow(clippy::too_many_arguments)]
async fn insert_transaction(
    conn: &mut SqliteConnection,
    id: &str,
    household_id: &str,
    transaction: &ParsedTransaction,
    identity: &str,
    identity_kind: &str,
    fingerprint: &str,
    revision: i64,
    now: &str,
) -> Result<bool, ImportError> {
    let result = sqlx::query(
        "INSERT INTO purchase_transactions \
         (id, household_id, retailer, purchase_date, source_kind, source_transaction_id, \
          transaction_identity, identity_kind, fingerprint, currency, total_cents, revision, \
          source_payload, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14) \
         ON CONFLICT(household_id, retailer, source_kind, transaction_identity) DO NOTHING",
    )
    .bind(id)
    .bind(household_id)
    .bind(&transaction.retailer)
    .bind(&transaction.purchase_date)
    .bind(SOURCE_KIND_IMPORT)
    .bind(&transaction.source_transaction_id)
    .bind(identity)
    .bind(identity_kind)
    .bind(fingerprint)
    .bind(&transaction.currency)
    .bind(transaction.total_cents)
    .bind(revision)
    .bind(&transaction.source_payload)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn insert_lines(
    conn: &mut SqliteConnection,
    transaction_id: &str,
    lines: &[ParsedLine],
    revision: i64,
    now: &str,
) -> Result<(), ImportError> {
    for (ordinal, line) in lines.iter().enumerate() {
        sqlx::query(
            "INSERT INTO purchase_lines \
             (id, transaction_id, line_ordinal, original_text, name, brand, quantity, size_value, \
              size_unit, unit_price_cents, line_total_cents, line_discount_cents, deposit_cents, \
              retailer_product_id, canonical_item_id, match_status, revision, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(transaction_id)
        .bind(ordinal as i64)
        .bind(&line.original_text)
        .bind(&line.name)
        .bind(&line.brand)
        .bind(line.quantity)
        .bind(line.size_value)
        .bind(&line.size_unit)
        .bind(line.unit_price_cents)
        .bind(line.line_total_cents)
        .bind(line.line_discount_cents)
        .bind(line.deposit_cents)
        .bind(&line.retailer_product_id)
        .bind(&line.canonical_item_id)
        .bind(&line.match_status)
        .bind(revision)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn fetch_line_rows(
    conn: &mut SqliteConnection,
    transaction_id: &str,
) -> Result<Vec<LineRow>, sqlx::Error> {
    sqlx::query_as::<_, LineRow>(
        "SELECT original_text, name, brand, quantity, size_value, size_unit, unit_price_cents, \
         line_total_cents, line_discount_cents, deposit_cents, retailer_product_id, \
         canonical_item_id, match_status \
         FROM purchase_lines WHERE transaction_id = ?1 ORDER BY line_ordinal",
    )
    .bind(transaction_id)
    .fetch_all(&mut *conn)
    .await
}

/// The comparable content of a stored transaction: the fields an import must
/// match for the write to be a no-op.
fn stored_content(stored: &StoredTransaction, lines: &[LineRow]) -> Value {
    json!({
        "retailer": stored.retailer,
        "purchase_date": stored.purchase_date,
        "currency": stored.currency,
        "total_cents": stored.total_cents,
        "lines": lines,
    })
}

/// The comparable content of a parsed transaction (identical shape to
/// [`stored_content`], so `serde_json::Value` equality is a content comparison).
fn parsed_content(transaction: &ParsedTransaction) -> Value {
    json!({
        "retailer": transaction.retailer,
        "purchase_date": transaction.purchase_date,
        "currency": transaction.currency,
        "total_cents": transaction.total_cents,
        "lines": transaction.lines,
    })
}

// ---------------------------------------------------------------------------
// Reads and deletes
// ---------------------------------------------------------------------------

/// List one household's transactions, oldest purchase date first.
pub async fn list_transactions(
    pool: &SqlitePool,
    household_id: &str,
) -> Result<Vec<TransactionSummary>, ImportError> {
    let rows = sqlx::query_as::<_, TransactionSummary>(
        "SELECT t.id, t.household_id, t.retailer, t.purchase_date, t.source_kind, \
                t.source_transaction_id, t.identity_kind, t.fingerprint, t.currency, \
                t.total_cents, t.revision, t.created_at, t.updated_at, \
                (SELECT COUNT(*) FROM purchase_lines pl WHERE pl.transaction_id = t.id) AS line_count \
         FROM purchase_transactions t WHERE t.household_id = ?1 \
         ORDER BY t.purchase_date, t.id",
    )
    .bind(household_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fetch one transaction with its lines and superseded revisions.
///
/// `None` when no such transaction belongs to `household_id` — a transaction of
/// another household is indistinguishable from a missing one, so the ledger is
/// never readable across households.
pub async fn get_transaction(
    pool: &SqlitePool,
    household_id: &str,
    transaction_id: &str,
) -> Result<Option<TransactionDetail>, ImportError> {
    let Some(transaction) = fetch_transaction_summary(pool, household_id, transaction_id).await?
    else {
        return Ok(None);
    };

    let lines = sqlx::query_as::<_, LineView>(
        "SELECT id, transaction_id, line_ordinal, original_text, name, brand, quantity, \
                size_value, size_unit, unit_price_cents, line_total_cents, line_discount_cents, \
                deposit_cents, retailer_product_id, canonical_item_id, match_status, revision, \
                created_at \
         FROM purchase_lines WHERE transaction_id = ?1 ORDER BY line_ordinal",
    )
    .bind(transaction_id)
    .fetch_all(pool)
    .await?;

    let revisions = sqlx::query_as::<_, RevisionRow>(
        "SELECT revision, retailer, purchase_date, currency, total_cents, source_payload, \
                lines_json, superseded_at \
         FROM purchase_transaction_revisions WHERE transaction_id = ?1 ORDER BY revision",
    )
    .bind(transaction_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| RevisionView {
        revision: row.revision,
        retailer: row.retailer,
        purchase_date: row.purchase_date,
        currency: row.currency,
        total_cents: row.total_cents,
        source_payload: row.source_payload,
        superseded_at: row.superseded_at,
        lines: serde_json::from_str(&row.lines_json).unwrap_or(Value::Null),
    })
    .collect();

    Ok(Some(TransactionDetail {
        transaction,
        lines,
        revisions,
    }))
}

#[derive(Debug, Clone, FromRow)]
struct RevisionRow {
    revision: i64,
    retailer: String,
    purchase_date: String,
    currency: String,
    total_cents: i64,
    source_payload: String,
    lines_json: String,
    superseded_at: String,
}

async fn fetch_transaction_summary(
    pool: &SqlitePool,
    household_id: &str,
    transaction_id: &str,
) -> Result<Option<TransactionSummary>, sqlx::Error> {
    sqlx::query_as::<_, TransactionSummary>(
        "SELECT t.id, t.household_id, t.retailer, t.purchase_date, t.source_kind, \
                t.source_transaction_id, t.identity_kind, t.fingerprint, t.currency, \
                t.total_cents, t.revision, t.created_at, t.updated_at, \
                (SELECT COUNT(*) FROM purchase_lines pl WHERE pl.transaction_id = t.id) AS line_count \
         FROM purchase_transactions t WHERE t.id = ?1 AND t.household_id = ?2",
    )
    .bind(transaction_id)
    .bind(household_id)
    .fetch_optional(pool)
    .await
}

/// Delete one transaction of `household_id`; its lines and revision history go
/// with it. Returns `false` when no such transaction belongs to the household,
/// so a cross-household delete changes nothing.
pub async fn delete_transaction(
    pool: &SqlitePool,
    household_id: &str,
    transaction_id: &str,
) -> Result<bool, ImportError> {
    let result =
        sqlx::query("DELETE FROM purchase_transactions WHERE id = ?1 AND household_id = ?2")
            .bind(transaction_id)
            .bind(household_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() > 0)
}
