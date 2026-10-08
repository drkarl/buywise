//! Tesco Clubcard purchase-history import (issue #107, roadmap #114).
//!
//! Turns a Tesco Clubcard data-export file into the household's **existing**
//! purchase ledger ([`crate::purchase_ledger`]) and feeds the frequency model
//! ([`crate::suggestions`]) so first-use suggestions can use months of history
//! instead of waiting weeks. It does not invent a second store: every purchase
//! becomes a normal `purchase_transactions` row with its lines, identity and
//! idempotency rules, exactly as `docs/purchase-ledger.md` documents.
//!
//! # The export
//!
//! The export is a JSON object with `requestId`, `purchases` and `orders`. Only
//! `purchases` is read; **`orders` is never read, stored or printed** because it
//! carries the delivery address and phone numbers, which are not the ledger's
//! business. Each purchase has `timestamp` (`YYYY-MM-DD HH:MM:SS[.fff]`), `type`
//! (`IN_STORE` or `GHS` for online grocery), `basketValueGross`,
//! `overallBasketSavings` (absent on some purchases), `basketValueNet`,
//! `numberOfItems`, `payment` and `items`. Each item has a `name` (one line in
//! the real export has none), `quantity`, `weight`, `price` and `volume`.
//!
//! # Mapping into the ledger
//!
//! One purchase → one ledger transaction:
//!
//! * `retailer` is always `tesco` (the store type is not a retailer).
//! * `purchase_date` is the calendar date of `timestamp` (the shop's own day).
//! * `total_cents` is `basketValueGross` — the amount paid. `basketValueNet` and
//!   `overallBasketSavings` are preserved in the transaction's `source_payload`,
//!   which is the original purchase object **verbatim**, so store type, both
//!   totals, the savings and every raw line number survive even though the
//!   ledger has no first-class column for them.
//! * Each item → one line: `original_text` and `name` are the item name as
//!   printed (an item with no name keeps a clearly-synthetic placeholder text),
//!   `quantity` is the unit count, `unit_price_cents` is the printed `price`,
//!   and `line_total_cents` is `price × volume` (the export's own line amount —
//!   `volume` is the unit count, or the weight for loose goods). The ledger
//!   trims `original_text`/`name`; the untrimmed text, and the `weight` and
//!   `volume` numbers, survive verbatim in the preserved `source_payload`.
//!
//! The export also itemises **refunds**: a whole refund purchase carries a
//! negative `basketValueGross` and `numberOfItems`, and a refund line inside an
//! ordinary basket carries a negative `quantity`/`volume` and therefore a
//! negative line total. Those are copied as-is — a negative `total_cents` and
//! `line_total_cents` are valid in the ledger — except that a line's `quantity`
//! is left unset for a non-positive count, because the ledger requires a
//! quantity greater than zero; the signed numbers survive in `source_payload`.
//!
//! The export's own numbers do not always reconcile (`numberOfItems` exceeds the
//! itemised lines on most purchases, and a basket's line amounts need not equal
//! its printed total), so the importer copies what each line says and never
//! invents or rebalances an amount. The gaps are recorded in
//! `docs/KNOWN_LIMITATIONS.md`.
//!
//! # Identity and idempotency
//!
//! The export has no per-purchase transaction id, so the importer supplies a
//! **stable computed key** as `source_transaction_id`: `tesco1:` plus the
//! SHA-256 of the timestamp, store type, both totals, the savings and every
//! line's name/quantity/weight/volume/price. Re-importing the same export is
//! therefore idempotent — the ledger's identity rule collapses it to one
//! transaction and one set of lines — while the key still distinguishes two
//! same-day purchases that differ in time, totals or content.
//!
//! # Canonical matching and frequency evidence
//!
//! Each line name is resolved with the existing Phase-0 cascade
//! ([`crate::matching::resolve`]: alias lookup then token/fuzzy match). A single
//! unambiguous match links the line (`canonical_item_id`, `match_status =
//! matched`); anything else — no match or several candidates — is left
//! `unmatched` and **kept**, never dropped or guessed, for the normal
//! clarification flow. No paid or external service is called.
//!
//! For every matched line with a positive line total in a purchase with a
//! non-negative gross total, the importer also writes a `purchase_events` row
//! with `source = 'import'` at the purchase's calendar day, so
//! [`crate::suggestions::compute_suggestions`] sees the imported history. The
//! event id is a deterministic hash of the household, the purchase identity and
//! the canonical item, and the write is `ON CONFLICT DO NOTHING`, so re-running
//! an import never duplicates evidence. The event carries no list or entry
//! reference (there was no list), which the schema already allows.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::matching::{self, ResolutionOutcome};
use crate::purchase_ledger::{self, ImportError};

/// The retailer token every imported purchase is stored under.
pub const TESCO_RETAILER: &str = "tesco";

/// Currency the export's amounts are in (Tesco Ireland).
pub const TESCO_CURRENCY: &str = "EUR";

/// Text stored for a line the export itemises without a name, so the line is
/// kept rather than dropped. The line's other numbers are preserved in the
/// transaction's `source_payload`; this placeholder is deliberately obviously
/// synthetic and never treated as a product name by matching.
pub const UNNAMED_LINE_TEXT: &str = "[unnamed item]";

/// Version tag on the computed purchase identity, so a future identity-rule
/// change can be told apart from a key computed today.
const IDENTITY_PREFIX: &str = "tesco1:";

/// Version tag on a `purchase_events` id, versioning the evidence id rule.
const EVENT_PREFIX: &str = "pe1:";

/// The only timestamp layout this build reads. `%.f` accepts the optional
/// fractional seconds the real export carries.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

/// Errors produced by the Tesco importer.
#[derive(Debug, thiserror::Error)]
pub enum TescoImportError {
    /// The export does not match the expected shape. Every problem is reported
    /// together, each naming its field path, so a file can be fixed in one pass.
    #[error("malformed Tesco export: {}", .0.join("; "))]
    Malformed(Vec<String>),

    /// The ledger refused the document it was built from (a link problem, an
    /// unsupported format version, or a database error).
    #[error(transparent)]
    Ledger(#[from] ImportError),

    /// A database error while resolving a line name to a canonical item.
    #[error(transparent)]
    Matching(#[from] crate::db::DbError),

    /// A database error while writing the frequency evidence.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One parsed, validated purchase.
#[derive(Debug, Clone, PartialEq)]
pub struct TescoPurchase {
    /// The export's `timestamp`, verbatim.
    pub timestamp: String,
    /// Calendar date of the purchase (`YYYY-MM-DD`), the shop's own day.
    pub date: String,
    /// Store type (`IN_STORE` or `GHS`), verbatim.
    pub store_type: String,
    /// `basketValueGross` in integer cents — the transaction total.
    pub gross_cents: i64,
    /// `basketValueNet` in integer cents (preserved in the payload).
    pub net_cents: i64,
    /// `overallBasketSavings` in integer cents (`0` when the field is absent).
    pub savings_cents: i64,
    /// The export's `numberOfItems`.
    pub number_of_items: i64,
    /// The purchase's lines, in export order.
    pub items: Vec<TescoItem>,
    /// The original purchase object, stored verbatim as `source_payload`.
    pub raw: Value,
}

/// One parsed, validated item line.
#[derive(Debug, Clone, PartialEq)]
pub struct TescoItem {
    /// The item name as printed, or `None` when the export itemises no name.
    pub name: Option<String>,
    /// Unit count.
    pub quantity: f64,
    /// Weight in the export's own unit (`0` when not a weighed item).
    pub weight: f64,
    /// Unit price in integer cents.
    pub unit_price_cents: i64,
    /// Unit count, or the weight for loose goods — the line-total multiplier.
    pub volume: f64,
    /// The line total in integer cents: `price × volume`, computed from the
    /// export's own unrounded price so no precision is lost rounding first.
    pub line_total_cents: i64,
}

/// Aggregate outcome of one Tesco import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TescoImportOutcome {
    /// Purchases in the document.
    pub purchases: usize,
    /// Transactions inserted.
    pub inserted: usize,
    /// Transactions corrected (revision bumped).
    pub updated: usize,
    /// Transactions left unchanged.
    pub unchanged: usize,
    /// Lines across every purchase.
    pub lines: usize,
    /// Lines linked to a canonical item.
    pub matched_lines: usize,
    /// Lines left `unmatched` and preserved.
    pub unmatched_lines: usize,
    /// `purchase_events` rows actually written this run.
    pub events_recorded: usize,
    /// Earliest purchase date in the document (`YYYY-MM-DD`).
    pub first_date: Option<String>,
    /// Latest purchase date in the document (`YYYY-MM-DD`).
    pub last_date: Option<String>,
}

/// Parse and validate a Tesco Clubcard export.
///
/// Structural and semantic problems are reported together, each naming its
/// field path. Only `purchases` is read: the `orders` section is never touched,
/// so its address and phone fields are neither read nor stored.
pub fn parse_tesco_export(document: &Value) -> Result<Vec<TescoPurchase>, TescoImportError> {
    let mut errors = Vec::new();
    let Some(root) = document.as_object() else {
        return Err(TescoImportError::Malformed(vec![
            "root: expected a JSON object".to_string(),
        ]));
    };

    let purchases: &[Value] = match root.get("purchases") {
        Some(Value::Array(items)) => items.as_slice(),
        Some(_) => {
            errors.push("purchases: expected an array".to_string());
            &[]
        }
        None => {
            errors.push("purchases: missing required field".to_string());
            &[]
        }
    };

    let mut parsed = Vec::with_capacity(purchases.len());
    for (index, value) in purchases.iter().enumerate() {
        let path = format!("purchases[{index}]");
        if let Some(purchase) = parse_purchase(value, &path, &mut errors) {
            parsed.push(purchase);
        }
    }

    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(TescoImportError::Malformed(errors))
    }
}

/// Import one Tesco export into `household_id`.
///
/// Parses and validates the export, maps each line name to a canonical item
/// with the existing cascade, imports the purchases through the shared ledger,
/// and records frequency evidence for matched positive-value lines in
/// non-negative-total purchases. `now` is the injected clock, used only as the
/// evidence rows' `recorded_at`.
///
/// The ledger import is one database transaction (all purchases or none). The
/// evidence write follows in its own transaction and is idempotent, so a failure
/// between the two leaves the ledger correct and is repaired by re-running.
pub async fn import_tesco_export(
    pool: &SqlitePool,
    household_id: &str,
    document: &Value,
    now: DateTime<Utc>,
) -> Result<TescoImportOutcome, TescoImportError> {
    let purchases = parse_tesco_export(document)?;

    // Resolve every distinct line name once. Matching is deterministic, so the
    // cache only avoids repeating the corpus scan for repeated names.
    let mut resolved: HashMap<String, Option<String>> = HashMap::new();
    for purchase in &purchases {
        for item in &purchase.items {
            let Some(name) = item.name.as_ref() else {
                continue;
            };
            if resolved.contains_key(name) {
                continue;
            }
            let matched = match matching::resolve(name, household_id, pool).await? {
                ResolutionOutcome::SingleMatch {
                    canonical_item_id, ..
                } => Some(canonical_item_id),
                ResolutionOutcome::MultipleCandidates { .. } | ResolutionOutcome::NoMatch => None,
            };
            resolved.insert(name.clone(), matched);
        }
    }

    let mut transactions = Vec::with_capacity(purchases.len());
    let mut evidence: Vec<(String, String, String)> = Vec::new();
    let mut lines = 0usize;
    let mut matched_lines = 0usize;
    let mut unmatched_lines = 0usize;
    let mut first_date: Option<String> = None;
    let mut last_date: Option<String> = None;

    for purchase in &purchases {
        let identity = purchase_identity(purchase);
        first_date = Some(match first_date {
            Some(current) if current <= purchase.date => current,
            _ => purchase.date.clone(),
        });
        last_date = Some(match last_date {
            Some(current) if current >= purchase.date => current,
            _ => purchase.date.clone(),
        });

        let mut seen_items: BTreeSet<String> = BTreeSet::new();
        let mut line_values = Vec::with_capacity(purchase.items.len());
        for item in &purchase.items {
            let canonical = item
                .name
                .as_ref()
                .and_then(|name| resolved.get(name).cloned().flatten());
            match &canonical {
                Some(canonical_item_id) => {
                    matched_lines += 1;
                    if purchase.gross_cents >= 0
                        && item.line_total_cents > 0
                        && seen_items.insert(canonical_item_id.clone())
                    {
                        evidence.push((
                            event_id(household_id, &identity, canonical_item_id),
                            canonical_item_id.clone(),
                            format!("{}T00:00:00Z", purchase.date),
                        ));
                    }
                }
                None => unmatched_lines += 1,
            }
            line_values.push(line_value(item, canonical.as_deref()));
            lines += 1;
        }

        transactions.push(json!({
            "source_transaction_id": identity,
            "retailer": TESCO_RETAILER,
            "purchase_date": purchase.date,
            "currency": TESCO_CURRENCY,
            "total_cents": purchase.gross_cents,
            "source_payload": purchase.raw,
            "lines": line_values,
        }));
    }

    let document = json!({ "format_version": 1, "transactions": transactions });
    let ledger = purchase_ledger::import_transactions(pool, household_id, &document).await?;

    let recorded_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut tx = pool.begin().await?;
    let mut events_recorded = 0usize;
    for (id, canonical_item_id, occurred_at) in &evidence {
        let result = sqlx::query(
            "INSERT INTO purchase_events \
                 (id, household_id, canonical_item_id, list_id, entry_id, source, occurred_at, recorded_at) \
             VALUES (?1, ?2, ?3, NULL, NULL, 'import', ?4, ?5) \
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(household_id)
        .bind(canonical_item_id)
        .bind(occurred_at)
        .bind(&recorded_at)
        .execute(&mut *tx)
        .await?;
        events_recorded += result.rows_affected() as usize;
    }
    tx.commit().await?;

    Ok(TescoImportOutcome {
        purchases: purchases.len(),
        inserted: ledger.inserted,
        updated: ledger.updated,
        unchanged: ledger.unchanged,
        lines,
        matched_lines,
        unmatched_lines,
        events_recorded,
        first_date,
        last_date,
    })
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn parse_purchase(value: &Value, path: &str, errors: &mut Vec<String>) -> Option<TescoPurchase> {
    let Some(map) = value.as_object() else {
        errors.push(format!("{path}: expected an object"));
        return None;
    };

    let timestamp = req_string(map, path, "timestamp", errors);
    let date = timestamp.as_deref().and_then(|raw| {
        match NaiveDateTime::parse_from_str(raw, TIMESTAMP_FORMAT) {
            Ok(parsed) => Some(parsed.date().format("%Y-%m-%d").to_string()),
            Err(_) => {
                errors.push(format!(
                    "{path}.timestamp: expected a \"YYYY-MM-DD HH:MM:SS\" timestamp"
                ));
                None
            }
        }
    });

    let store_type = req_string(map, path, "type", errors);
    let gross_cents = req_cents(map, path, "basketValueGross", errors);
    let net_cents = req_cents(map, path, "basketValueNet", errors);
    let savings_cents = opt_cents(map, path, "overallBasketSavings", errors).unwrap_or(0);
    let number_of_items = req_i64(map, path, "numberOfItems", errors);

    let items = match map.get("items") {
        Some(Value::Array(values)) => {
            let mut parsed = Vec::with_capacity(values.len());
            for (index, item) in values.iter().enumerate() {
                let item_path = format!("{path}.items[{index}]");
                if let Some(parsed_item) = parse_item(item, &item_path, errors) {
                    parsed.push(parsed_item);
                }
            }
            Some(parsed)
        }
        Some(_) => {
            errors.push(format!("{path}.items: expected an array"));
            None
        }
        None => {
            errors.push(format!("{path}.items: missing required field"));
            None
        }
    };

    Some(TescoPurchase {
        timestamp: timestamp?,
        date: date?,
        store_type: store_type?,
        gross_cents: gross_cents?,
        net_cents: net_cents?,
        savings_cents,
        number_of_items: number_of_items?,
        items: items?,
        raw: value.clone(),
    })
}

fn parse_item(value: &Value, path: &str, errors: &mut Vec<String>) -> Option<TescoItem> {
    let Some(map) = value.as_object() else {
        errors.push(format!("{path}: expected an object"));
        return None;
    };

    let name = opt_string(map, path, "name", errors);
    let quantity = req_number(map, path, "quantity", errors);
    let weight = req_number(map, path, "weight", errors);
    let price = req_number(map, path, "price", errors);
    let unit_price_cents = price.and_then(to_cents);
    let volume = req_number(map, path, "volume", errors);
    let line_total_cents = price
        .zip(volume)
        .and_then(|(price, volume)| to_cents(price * volume));
    if price.is_some() && line_total_cents.is_none() {
        errors.push(format!("{path}.price: line amount out of range"));
    }

    Some(TescoItem {
        name,
        quantity: quantity?,
        weight: weight?,
        unit_price_cents: unit_price_cents?,
        volume: volume?,
        line_total_cents: line_total_cents?,
    })
}

/// Build one ledger line from an item and its (optional) canonical match.
fn line_value(item: &TescoItem, canonical_item_id: Option<&str>) -> Value {
    let original_text = item
        .name
        .clone()
        .unwrap_or_else(|| UNNAMED_LINE_TEXT.to_string());
    let quantity = (item.quantity > 0.0).then_some(item.quantity);
    let match_status = if canonical_item_id.is_some() {
        purchase_ledger::MATCH_MATCHED
    } else {
        purchase_ledger::MATCH_UNMATCHED
    };
    json!({
        "original_text": original_text,
        "name": item.name,
        "quantity": quantity,
        "unit_price_cents": item.unit_price_cents,
        "line_total_cents": item.line_total_cents,
        "canonical_item_id": canonical_item_id,
        "match_status": match_status,
    })
}

/// The stable identity for one purchase, used as its `source_transaction_id`.
///
/// `tesco1:` plus the SHA-256 of the timestamp, store type, both totals, the
/// savings, and every line's name, quantity, weight, volume and price — the
/// "timestamp plus totals plus line content" the identity must survive.
fn purchase_identity(purchase: &TescoPurchase) -> String {
    let mut hasher = Sha256::new();
    hasher.update(purchase.timestamp.as_bytes());
    hasher.update(b"\n");
    hasher.update(purchase.store_type.as_bytes());
    hasher.update(b"\n");
    for value in [
        purchase.gross_cents,
        purchase.net_cents,
        purchase.savings_cents,
        purchase.number_of_items,
    ] {
        hasher.update(value.to_string().as_bytes());
        hasher.update(b"\n");
    }
    for item in &purchase.items {
        hasher.update(item.name.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\n");
        for value in [item.quantity, item.weight, item.volume] {
            hasher.update(format!("{value}").as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(item.unit_price_cents.to_string().as_bytes());
        hasher.update(b"\n");
    }
    format!("{IDENTITY_PREFIX}{}", hex::encode(hasher.finalize()))
}

/// The deterministic `purchase_events.id` for one (purchase, canonical item).
fn event_id(household_id: &str, identity: &str, canonical_item_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(household_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(identity.as_bytes());
    hasher.update(b"\n");
    hasher.update(canonical_item_id.as_bytes());
    format!("{EVENT_PREFIX}{}", hex::encode(hasher.finalize()))
}

/// A required non-empty string.
fn req_string(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    match map.get(key) {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.clone()),
        Some(Value::String(_)) => {
            errors.push(format!("{path}.{key}: expected a non-empty string"));
            None
        }
        Some(_) => {
            errors.push(format!("{path}.{key}: expected a string"));
            None
        }
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            None
        }
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
        Some(Value::String(text)) if text.trim().is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            errors.push(format!("{path}.{key}: expected a string"));
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
        Some(Value::Number(number)) => match number.as_i64() {
            Some(value) => Some(value),
            None => {
                errors.push(format!("{path}.{key}: expected an integer"));
                None
            }
        },
        Some(_) => {
            errors.push(format!("{path}.{key}: expected an integer"));
            None
        }
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            None
        }
    }
}

/// A required number of money, in integer cents.
fn req_cents(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<i64> {
    match map.get(key) {
        Some(Value::Number(number)) => match number.as_f64().and_then(to_cents) {
            Some(value) => Some(value),
            None => {
                errors.push(format!("{path}.{key}: expected an amount"));
                None
            }
        },
        Some(_) => {
            errors.push(format!("{path}.{key}: expected an amount"));
            None
        }
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            None
        }
    }
}

/// An optional number of money, in integer cents.
fn opt_cents(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<i64> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::Number(number)) => match number.as_f64().and_then(to_cents) {
            Some(value) => Some(value),
            None => {
                errors.push(format!("{path}.{key}: expected an amount"));
                None
            }
        },
        Some(_) => {
            errors.push(format!("{path}.{key}: expected an amount"));
            None
        }
    }
}

/// A required finite number. Sign is allowed: the export itemises refunds with a
/// negative quantity, volume and total.
fn req_number(
    map: &Map<String, Value>,
    path: &str,
    key: &str,
    errors: &mut Vec<String>,
) -> Option<f64> {
    match map.get(key) {
        Some(Value::Number(number)) => match number.as_f64() {
            Some(value) if value.is_finite() => Some(value),
            _ => {
                errors.push(format!("{path}.{key}: expected a number"));
                None
            }
        },
        Some(_) => {
            errors.push(format!("{path}.{key}: expected a number"));
            None
        }
        None => {
            errors.push(format!("{path}.{key}: missing required field"));
            None
        }
    }
}

/// Round a decimal amount to integer cents, refusing values no money column
/// could hold. `None` for a non-finite or absurd value.
fn to_cents(amount: f64) -> Option<i64> {
    if !amount.is_finite() || amount.abs() >= 1e13 {
        return None;
    }
    Some((amount * 100.0).round() as i64)
}
