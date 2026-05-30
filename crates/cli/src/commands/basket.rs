//! Shared basket pre-step: resolve raw lines and build [`OptimiserInput`].
//!
//! This is the "caller responsibility" pre-step the optimiser documents
//! (`crates/core/src/optimiser.rs`): for each raw line we
//!
//! 1. parse count/size ([`parse_count_and_size`]),
//! 2. resolve the residual text to a canonical item ([`resolve`]),
//! 3. load the canonical item's `default_constraint_policy` and its
//!    `item_constraints` row, and
//! 4. merge the parsed requested size into those constraints (so `"Fairy 500ml"`
//!    keeps both `brand='Fairy'` and a preferred size).
//!
//! The result is an [`OptimiserInput`] the optimiser can price directly.

use anyhow::Result;
use buywise_core::db;
use buywise_core::matching::{resolve, ResolutionOutcome};
use buywise_core::normalise::parse_count_and_size;
use buywise_core::optimiser::{BasketItem, OptimiserInput};
use buywise_core::{ConstraintPolicy, ItemConstraints, RequestedSize, Source};
use sqlx::sqlite::SqlitePool;

/// How one raw basket line resolved, for the resolution table and basket build.
#[derive(Debug, Clone)]
pub struct ResolvedLine {
    /// The original raw text.
    pub raw: String,
    /// Normalised residual text (count/size stripped).
    pub text: String,
    /// Parsed count.
    pub count: u32,
    /// Parsed requested size, if any.
    pub requested_size: Option<RequestedSize>,
    /// The resolution outcome.
    pub outcome: ResolutionOutcome,
}

impl ResolvedLine {
    /// A short human-readable status for the resolution table.
    pub fn status(&self) -> &'static str {
        match &self.outcome {
            ResolutionOutcome::SingleMatch { .. } => "resolved",
            ResolutionOutcome::MultipleCandidates { .. } => "ambiguous",
            ResolutionOutcome::NoMatch => "unresolved",
        }
    }

    /// The resolved canonical item id, if (and only if) the line resolved to a
    /// single unambiguous item. Ambiguous and no-match lines are unresolved for
    /// pricing purposes (Phase 0 has no clarification step).
    pub fn canonical_item_id(&self) -> Option<&str> {
        match &self.outcome {
            ResolutionOutcome::SingleMatch { canonical_item_id } => Some(canonical_item_id),
            _ => None,
        }
    }
}

/// Resolve every raw line against the household catalogue in `pool`.
pub async fn resolve_lines(
    pool: &SqlitePool,
    household_id: &str,
    raw_lines: &[String],
) -> Result<Vec<ResolvedLine>> {
    let mut out = Vec::with_capacity(raw_lines.len());
    for raw in raw_lines {
        let parsed = parse_count_and_size(raw);
        // Resolve on the residual text (count/size stripped). An empty residual
        // (a quantity-only line) cannot resolve, so it is left as NoMatch.
        let outcome = if parsed.text.is_empty() {
            ResolutionOutcome::NoMatch
        } else {
            resolve(&parsed.text, household_id, pool).await?
        };
        out.push(ResolvedLine {
            raw: raw.clone(),
            text: parsed.text,
            count: parsed.count,
            requested_size: parsed.requested_size,
            outcome,
        });
    }
    Ok(out)
}

/// Build an [`OptimiserInput`] from resolved lines, loading each resolved item's
/// constraint policy and `item_constraints` row and merging the requested size.
pub async fn build_optimiser_input(
    pool: &SqlitePool,
    household_id: &str,
    lines: &[ResolvedLine],
    threshold_2_cents: Option<i64>,
) -> Result<OptimiserInput> {
    let mut items = Vec::with_capacity(lines.len());
    for line in lines {
        let canonical_item_id = line.canonical_item_id().map(str::to_string);

        // Policy + constraints are only meaningful for a resolved item.
        let (policy, constraints) = match canonical_item_id.as_deref() {
            Some(id) => {
                let policy = db::canonical_item_policy(pool, id)
                    .await?
                    .unwrap_or(ConstraintPolicy::Any);
                let mut constraints = db::item_constraints_by_canonical_id(pool, id).await?;
                merge_requested_size(&mut constraints, id, line.requested_size.as_ref());
                (policy, constraints)
            }
            None => (ConstraintPolicy::Any, None),
        };

        items.push(BasketItem {
            display_text: line.raw.clone(),
            canonical_item_id,
            count: line.count,
            constraint_policy: policy,
            constraints,
        });
    }

    // Freshness needs the current BasketWatch run number (None ⇒ Unknown).
    let current_run = db::current_source_run_number(pool, Source::BasketWatch).await?;

    let mut input = OptimiserInput::new(household_id, current_run, items);
    if let Some(threshold) = threshold_2_cents {
        input.threshold_2_cents = threshold;
    }
    Ok(input)
}

/// Merge a parsed requested size into an item's constraints, so a per-line size
/// (e.g. `"milk 2L"`) is enforced even when the stored constraints carry none.
///
/// The requested size takes precedence over any stored `preferred_size`: it is
/// the user's explicit intent for this line. When the item has no stored
/// constraints row but a size was requested, a minimal constraints object is
/// synthesised to carry it.
fn merge_requested_size(
    constraints: &mut Option<ItemConstraints>,
    canonical_item_id: &str,
    requested_size: Option<&RequestedSize>,
) {
    let Some(size) = requested_size else {
        return;
    };
    match constraints {
        Some(c) => {
            c.preferred_size_value = Some(size.value);
            c.preferred_size_basis = Some(size.basis);
        }
        None => {
            *constraints = Some(ItemConstraints {
                id: format!("cli-requested-size:{canonical_item_id}"),
                canonical_item_id: canonical_item_id.to_string(),
                brand: None,
                allowed_brands: None,
                excluded_brands: None,
                preferred_size_value: Some(size.value),
                preferred_size_basis: Some(size.basis),
                min_size_value: None,
                min_size_basis: None,
                max_size_value: None,
                max_size_basis: None,
                pack_count: None,
                form_factor: None,
                dietary_flags: None,
                substitution_policy: buywise_core::SubstitutionPolicy::Any,
                updated_at: String::new(),
            });
        }
    }
}
