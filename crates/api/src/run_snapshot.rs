//! Frozen recommendation-run evidence snapshot (issue #117, roadmap #114;
//! savings slice 2).
//!
//! Slice 1 (migration 016) made savings labels honest and the aggregates
//! verified-only. This module defines the immutable evidence a run freezes so a
//! later comparison against what was actually bought is never rebuilt from
//! mutable `list_entries` rows or from offers refreshed since the run.
//!
//! Three JSON payloads are written once, at run creation, into migration-018
//! columns on `recommendation_runs` (see `migrations/018_*.sql`):
//!
//!   * [`BasketSnapshot`] — the input basket exactly as the optimiser saw it;
//!   * [`PriceEvidence`] — one row per offer the run used (the offers backing the
//!     best-single and recommended plans), with its observed date and pack size;
//!   * [`CoverageSnapshot`] — which lines each plan priced, the common-scope set
//!     priced by both, and the bounded covered-subtotal figure.
//!
//! [`snapshot_digest`] hashes the three payloads so the frozen evidence can be
//! checked for tampering later. Old runs (pre-018) carry NULL columns and are
//! read as "no frozen evidence" everywhere.
//!
//! Pure module: the DB reads that populate [`PriceEvidence`] live in
//! `optimise.rs`, and the response/ledger shapes derive from these types.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use buywise_core::domain::{
    Cents, ConstraintPolicy, ItemConstraints, Retailer, SizeBasis, StoreStatus,
};
use buywise_core::optimiser::{OptimiserInput, OptimiserOutput, StorePlan, StoreSetting};

/// Snapshot schema version written into `recommendation_runs.snapshot_version`
/// and round-tripped inside each payload, so a later reader can tell which
/// layout a row carries.
pub const SNAPSHOT_VERSION: i64 = 1;

/// Coverage status: both plans exist and price the same lines, so the full-basket
/// subtraction is like-for-like.
pub const COVERAGE_COMPARABLE: &str = "comparable";
/// Coverage status: both plans exist but price different lines, so the
/// full-basket subtraction is suppressed and only a bounded covered subtotal is
/// shown.
pub const COVERAGE_UNEQUAL: &str = "unequal_coverage";
/// Coverage status: there is no best-single/recommended comparison to make (no
/// plan priced anything), so no figure is stated.
pub const COVERAGE_NO_PLAN: &str = "no_plan";

// ---------------------------------------------------------------------------
// Basket snapshot
// ---------------------------------------------------------------------------

/// A base-unit size the household's constraints define for an entry (the
/// `item_constraints` preferred size), where known. Together with an offer's
/// recorded pack size this is what a later reconciliation needs to convert packs
/// to comparable base units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseUnit {
    /// The size value in `basis` units (e.g. `1000.0` for 1000 ml).
    pub value: f64,
    /// The base unit (`g`, `ml` or `each`).
    pub basis: SizeBasis,
}

/// One frozen basket line: the ACTIVE list entry the optimiser was given, with
/// everything needed to recognise it after the list has changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasketSnapshotEntry {
    /// Index into the optimiser input (the identity plan lines carry).
    pub item_index: usize,
    /// The source `list_entries.id`, when the caller had a persisted line.
    pub list_entry_id: Option<String>,
    /// The resolved canonical item id, or `None` when the entry was unresolved.
    pub canonical_item_id: Option<String>,
    /// The household's display text for the line.
    pub display_text: String,
    /// Requested pack count (`list_entries.quantity`, clamped to ≥ 1).
    pub requested_packs: u32,
    /// The canonical item's constraint strictness the optimiser applied.
    pub constraint_policy: ConstraintPolicy,
    /// The item's product rules (`item_constraints`), frozen as applied.
    pub constraints: Option<ItemConstraints>,
    /// The constrained base-unit size, where known.
    pub base_unit: Option<BaseUnit>,
}

/// The frozen input basket for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasketSnapshot {
    /// Snapshot schema version.
    pub version: i64,
    /// The optimised list.
    pub list_id: String,
    /// The owning household.
    pub household_id: String,
    /// One entry per input basket line, in optimiser order.
    pub entries: Vec<BasketSnapshotEntry>,
    /// Effective store settings used by the optimiser, including defaults for
    /// retailers with no stored setting.
    #[serde(default)]
    pub store_settings: Vec<StoreSetting>,
    /// Occasional stores admitted for this run.
    #[serde(default)]
    pub trip_retailers: Vec<Retailer>,
}

/// Build the basket snapshot from the exact [`OptimiserInput`] the run used.
pub fn build_basket_snapshot(
    list_id: &str,
    household_id: &str,
    input: &OptimiserInput,
) -> BasketSnapshot {
    let entries = input
        .items
        .iter()
        .enumerate()
        .map(|(item_index, item)| BasketSnapshotEntry {
            item_index,
            list_entry_id: item.entry_id.clone(),
            canonical_item_id: item.canonical_item_id.clone(),
            display_text: item.display_text.clone(),
            requested_packs: item.count,
            constraint_policy: item.constraint_policy,
            constraints: item.constraints.clone(),
            base_unit: base_unit_of(item.constraints.as_ref()),
        })
        .collect();
    BasketSnapshot {
        version: SNAPSHOT_VERSION,
        list_id: list_id.to_string(),
        household_id: household_id.to_string(),
        entries,
        store_settings: Retailer::ALL
            .iter()
            .map(|&retailer| {
                input
                    .store_settings
                    .iter()
                    .find(|setting| setting.retailer == retailer)
                    .cloned()
                    .unwrap_or(StoreSetting {
                        retailer,
                        status: StoreStatus::Active,
                        nearby: false,
                        category_rules: Vec::new(),
                    })
            })
            .collect(),
        trip_retailers: input.trip_retailers.clone(),
    }
}

/// The constrained base-unit size of an item, where the constraints define a
/// preferred size (value + basis). `None` when there is no constraints row or no
/// preferred size.
fn base_unit_of(constraints: Option<&ItemConstraints>) -> Option<BaseUnit> {
    let constraints = constraints?;
    let value = constraints.preferred_size_value?;
    let basis = constraints.preferred_size_basis?;
    Some(BaseUnit { value, basis })
}

// ---------------------------------------------------------------------------
// Price evidence
// ---------------------------------------------------------------------------

/// One frozen offer the run used: everything needed to re-derive the price the
/// household was shown, without re-reading `product_offers` (which gains rows
/// every refresh).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfferEvidence {
    /// `product_offers.id`.
    pub offer_id: String,
    /// The retailer that offer is sold at.
    pub retailer: Retailer,
    /// `retailer_products.id` the offer belongs to.
    pub retailer_product_id: String,
    /// Feed or source that supplied the retailer product, when known.
    #[serde(default)]
    pub source: Option<String>,
    /// The product's display name at run time.
    pub product_name: String,
    /// The offer's `source_snapshot_date` — the date the price was observed.
    pub observed_date: Option<String>,
    /// Shelf price in cents.
    pub shelf_price_cents: Cents,
    /// Loyalty price in cents, when the offer carries one (the assumed-paid
    /// price; loyalty schemes are assumed active).
    pub loyalty_price_cents: Option<Cents>,
    /// Loyalty scheme backing the loyalty price (`clubcard`/`real_rewards`).
    pub loyalty_scheme: Option<String>,
    /// Source unit price in micro-euros, when known.
    pub unit_price_eur_micros: Option<i64>,
    /// Basis the unit price is quoted against (e.g. `per_kg`), when known.
    pub unit_price_basis: Option<String>,
    /// Human-readable pack size text (e.g. "2L"), when known.
    pub pack_size_text: Option<String>,
    /// Numeric pack size, when known.
    pub size_value: Option<f64>,
    /// Unit of [`Self::size_value`], when known.
    pub size_unit: Option<String>,
    /// Number of units in the pack, when known.
    pub pack_count: Option<i32>,
    /// Total usable base units, when retailer size semantics define them.
    #[serde(default)]
    pub usable_base_units: Option<BaseUnit>,
    /// The offer's previous price, the discount assumption it may carry.
    pub previous_price_cents: Option<Cents>,
    /// Free-text offer description (e.g. a promotion), when present.
    pub offer_text: Option<String>,
    /// The offer's validity end date, when present.
    pub offer_valid_until: Option<String>,
}

/// The frozen price evidence for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceEvidence {
    /// Snapshot schema version.
    pub version: i64,
    /// One row per offer used, ordered by offer id.
    pub offers: Vec<OfferEvidence>,
}

/// The distinct offer ids the run used, sorted: the offers backing the
/// best-single and recommended plans (the two plans the savings figure compares).
///
/// The comparison scope is deliberate — these are the offers behind the figures
/// the product presents, so freezing them is what makes the covered subtotal and
/// the full-basket saving defensible. Benchmark/diagnostic plans
/// (`best_two_store`, `theoretical_minimum`) are not part of any claimed saving
/// and so are not required evidence.
pub fn used_offer_ids(output: &OptimiserOutput) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for plan in [
        output.best_single.as_ref(),
        output.recommended.as_ref().map(|r| &r.plan),
    ]
    .into_iter()
    .flatten()
    {
        for line in &plan.lines {
            ids.insert(line.offer_id.clone());
        }
    }
    ids
}

/// The oldest observed date among the given offers (NULLs ignored), or `None`
/// when no offer carries one.
///
/// ISO-8601 dates sort lexicographically, so the minimum is the oldest. The run's
/// `offer_snapshot_date` uses this: a basket compiled from a mixture of evidence
/// is only as fresh as its stalest line, so the OLDEST date is the honest one.
pub fn oldest_observed_date(offers: &[OfferEvidence]) -> Option<String> {
    offers.iter().filter_map(|o| o.observed_date.clone()).min()
}

/// Number of used offers whose observed date is unknown.
pub fn undated_offer_count(offers: &[OfferEvidence]) -> usize {
    offers
        .iter()
        .filter(|offer| offer.observed_date.is_none())
        .count()
}

// ---------------------------------------------------------------------------
// Coverage snapshot
// ---------------------------------------------------------------------------

/// The frozen coverage for one run: which lines each plan priced, the common
/// scope priced by both, and the bounded covered-subtotal figure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageSnapshot {
    /// Snapshot schema version.
    pub version: i64,
    /// Total ACTIVE basket lines submitted to the optimiser (the label's
    /// denominator).
    pub basket_lines: usize,
    /// Whether a best-single plan exists.
    #[serde(default)]
    pub has_best_single: bool,
    /// Whether a recommended plan exists.
    #[serde(default)]
    pub has_recommended: bool,
    /// Item indices the recommended plan prices.
    pub recommended_priced: Vec<usize>,
    /// Item indices the best-single plan prices.
    pub best_single_priced: Vec<usize>,
    /// Item indices priced by BOTH plans (the common scope).
    pub common_scope: Vec<usize>,
    /// Item indices only the best-single plan prices.
    pub best_single_only: Vec<usize>,
    /// Item indices only the recommended plan prices.
    pub recommended_only: Vec<usize>,
    /// Item indices resolved but priceable at no store (`no_valid_offer_items`).
    pub missing: Vec<usize>,
    /// Item indices that could not be resolved (`unresolved_items`).
    pub unresolved: Vec<usize>,
    /// Item indices held for household confirmation (`needs_confirmation_items`).
    pub confirmation_needed: Vec<usize>,
    /// The recommended plan's cost over [`Self::common_scope`], or `None` when
    /// nothing is commonly priced.
    pub covered_subtotal_cents: Option<Cents>,
    /// `best_single − recommended` over the common scope (the bounded estimate),
    /// or `None` when nothing is commonly priced.
    pub covered_saving_cents: Option<Cents>,
    /// `best_single.total − recommended.total` when coverage is comparable,
    /// otherwise `None` (the full-basket subtraction is suppressed).
    pub full_basket_saving_cents: Option<Cents>,
    /// The oldest observed date among the offers used, for the "prices as of"
    /// label. Mirrors `recommendation_runs.offer_snapshot_date`.
    pub price_date: Option<String>,
    /// Number of used offers without an observed date.
    #[serde(default)]
    pub undated_offer_count: usize,
}

impl CoverageSnapshot {
    /// Whether the two plans price exactly the same lines.
    pub fn is_comparable(&self) -> bool {
        self.best_single_only.is_empty() && self.recommended_only.is_empty()
    }

    /// The coverage status code (`comparable` / `unequal_coverage` / `no_plan`).
    pub fn status(&self) -> &'static str {
        if !self.has_best_single || !self.has_recommended {
            COVERAGE_NO_PLAN
        } else if self.is_comparable() {
            COVERAGE_COMPARABLE
        } else {
            COVERAGE_UNEQUAL
        }
    }
}

/// Compute the coverage snapshot from the plans the run produced.
///
/// `basket_lines` is the count of ACTIVE lines submitted. `price_date` is the
/// oldest observed date among the offers used (see [`oldest_observed_date`]).
pub fn build_coverage(
    output: &OptimiserOutput,
    basket_lines: usize,
    price_date: Option<String>,
    undated_offer_count: usize,
) -> CoverageSnapshot {
    let best_single = output.best_single.as_ref();
    let recommended = output.recommended.as_ref().map(|r| &r.plan);

    let best_single_priced = priced_indices(best_single);
    let recommended_priced = priced_indices(recommended);
    let best_set: BTreeSet<usize> = best_single_priced.iter().copied().collect();
    let rec_set: BTreeSet<usize> = recommended_priced.iter().copied().collect();
    let common_scope: Vec<usize> = best_set.intersection(&rec_set).copied().collect();
    let best_single_only: Vec<usize> = best_set.difference(&rec_set).copied().collect();
    let recommended_only: Vec<usize> = rec_set.difference(&best_set).copied().collect();

    // Totals over the common scope (both plans have exactly these lines).
    let recommended_common: Cents = common_total(recommended, &common_scope);
    let best_common: Cents = common_total(best_single, &common_scope);
    let covered_subtotal_cents = (!common_scope.is_empty()).then_some(recommended_common);
    let covered_saving_cents =
        (!common_scope.is_empty()).then_some(best_common - recommended_common);

    let has_best_single = best_single.is_some();
    let has_recommended = recommended.is_some();
    let full_basket_saving_cents = match (best_single, recommended) {
        (Some(b), Some(r)) if best_single_only.is_empty() && recommended_only.is_empty() => {
            Some(b.total_cents - r.total_cents)
        }
        _ => None,
    };

    CoverageSnapshot {
        version: SNAPSHOT_VERSION,
        basket_lines,
        has_best_single,
        has_recommended,
        recommended_priced,
        best_single_priced,
        common_scope,
        best_single_only,
        recommended_only,
        missing: output
            .no_valid_offer_items
            .iter()
            .map(|i| i.item_index)
            .collect(),
        unresolved: output
            .unresolved_items
            .iter()
            .map(|i| i.item_index)
            .collect(),
        confirmation_needed: output
            .needs_confirmation_items
            .iter()
            .map(|i| i.item_index)
            .collect(),
        covered_subtotal_cents,
        covered_saving_cents,
        full_basket_saving_cents,
        price_date,
        undated_offer_count,
    }
}

/// The item indices a plan prices, sorted and de-duplicated.
fn priced_indices(plan: Option<&StorePlan>) -> Vec<usize> {
    let Some(plan) = plan else {
        return Vec::new();
    };
    plan.lines
        .iter()
        .map(|l| l.item_index)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The sum of `line_total_cents` over the given item indices for a plan.
fn common_total(plan: Option<&StorePlan>, indices: &[usize]) -> Cents {
    let Some(plan) = plan else {
        return 0;
    };
    let want: BTreeSet<usize> = indices.iter().copied().collect();
    plan.lines
        .iter()
        .filter(|l| want.contains(&l.item_index))
        .map(|l| l.line_total_cents)
        .sum()
}

// ---------------------------------------------------------------------------
// Response / ledger summary
// ---------------------------------------------------------------------------

/// The coverage result surfaced on the optimise response and, from the stored
/// snapshot, on the savings ledger. Carries the labelled, bounded figure a
/// household can trust: never a full-basket saving on unequal coverage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageSummary {
    /// `comparable` / `unequal_coverage` / `no_plan`.
    pub status: String,
    /// Short label naming the coverage and evidence date, e.g.
    /// `"Covered subtotal: 8 of 10 lines, prices as of 2026-06-01"`.
    pub label: String,
    /// Lines priced by both plans (the numerator).
    pub covered_lines: usize,
    /// Active basket lines submitted (the denominator).
    pub total_lines: usize,
    /// The recommended plan's cost over the common scope, or `None`.
    pub covered_subtotal_cents: Option<Cents>,
    /// `best_single − recommended` over the common scope, or `None`.
    pub covered_saving_cents: Option<Cents>,
    /// The full-basket saving when coverage is comparable; `None` (suppressed)
    /// otherwise.
    pub full_basket_saving_cents: Option<Cents>,
    /// The oldest observed offer date among the offers used, or `None`.
    pub price_date: Option<String>,
    /// Number of used offers without an observed date.
    pub undated_offer_count: usize,
}

impl CoverageSummary {
    /// Derive the response/ledger shape from a stored (or freshly built) coverage
    /// snapshot.
    pub fn from_snapshot(snapshot: &CoverageSnapshot) -> Self {
        let status = snapshot.status();
        Self {
            label: covered_subtotal_label(
                snapshot.common_scope.len(),
                snapshot.basket_lines,
                snapshot.price_date.as_deref(),
                snapshot.undated_offer_count,
            ),
            status: status.to_string(),
            covered_lines: snapshot.common_scope.len(),
            total_lines: snapshot.basket_lines,
            covered_subtotal_cents: snapshot.covered_subtotal_cents,
            covered_saving_cents: snapshot.covered_saving_cents,
            full_basket_saving_cents: snapshot.full_basket_saving_cents,
            price_date: snapshot.price_date.clone(),
            undated_offer_count: snapshot.undated_offer_count,
        }
    }

    /// Build the summary for a fresh optimise response. Equivalent to
    /// [`build_coverage`] followed by [`Self::from_snapshot`].
    pub fn from_plans(
        output: &OptimiserOutput,
        basket_lines: usize,
        price_date: Option<String>,
        undated_offer_count: usize,
    ) -> Self {
        Self::from_snapshot(&build_coverage(
            output,
            basket_lines,
            price_date,
            undated_offer_count,
        ))
    }
}

/// The short covered-subtotal label. Undated used offers are named explicitly.
pub fn covered_subtotal_label(
    covered: usize,
    total: usize,
    price_date: Option<&str>,
    undated_offer_count: usize,
) -> String {
    let mut label = format!("Covered subtotal: {covered} of {total} lines");
    if let Some(date) = price_date {
        label.push_str(&format!(", prices as of {date}"));
    } else if undated_offer_count > 0 {
        label.push_str(", prices as of unknown");
    }
    if undated_offer_count > 0 {
        let noun = if undated_offer_count == 1 {
            "offer"
        } else {
            "offers"
        };
        label.push_str(&format!(", {undated_offer_count} {noun} undated"));
    }
    label
}

// ---------------------------------------------------------------------------
// Digest
// ---------------------------------------------------------------------------

/// SHA-256 hex digest over the three frozen payloads, so a later reader can
/// detect tampering. The digest is over the exact serialised strings written to
/// the columns, joined by `\n`, so it never depends on serialisation order.
pub fn snapshot_digest(basket_json: &str, price_json: &str, coverage_json: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(basket_json.as_bytes());
    hasher.update(b"\n");
    hasher.update(price_json.as_bytes());
    hasher.update(b"\n");
    hasher.update(coverage_json.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use buywise_core::optimiser::{PlanLine, Recommendation, RecommendedKind};

    fn line(item_index: usize, total: Cents) -> PlanLine {
        PlanLine {
            item_index,
            entry_id: None,
            display_text: format!("line {item_index}"),
            product_name: format!("product {item_index}"),
            canonical_item_id: Some(format!("ci-{item_index}")),
            retailer: Retailer::Tesco,
            retailer_product_id: format!("rp-{item_index}"),
            offer_id: format!("of-{item_index}"),
            unit_price_cents: total,
            line_total_cents: total,
            loyalty_applied: false,
            freshness_confidence: buywise_core::domain::FreshnessConfidence::High,
        }
    }

    fn plan(indices: &[(usize, Cents)]) -> StorePlan {
        StorePlan {
            stores: vec![Retailer::Tesco],
            lines: indices.iter().map(|(i, t)| line(*i, *t)).collect(),
            total_cents: indices.iter().map(|(_, t)| *t).sum(),
            missing_count: 0,
            dunnes_voucher_offer_text: None,
        }
    }

    fn output(best: Option<StorePlan>, recommended: Option<StorePlan>) -> OptimiserOutput {
        OptimiserOutput {
            basket_items: Vec::new(),
            best_single: best,
            best_two_store: None,
            theoretical_minimum: None,
            recommended: recommended.map(|plan| Recommendation {
                kind: RecommendedKind::SingleStore,
                plan,
                clears_convenience_threshold: false,
                two_store_saving_cents: 0,
            }),
            unresolved_items: Vec::new(),
            no_valid_offer_items: Vec::new(),
            needs_confirmation_items: Vec::new(),
            reasons: Vec::new(),
            aldi_comparison: None,
        }
    }

    #[test]
    fn equal_coverage_keeps_the_full_basket_saving() {
        let out = output(
            Some(plan(&[(0, 300), (1, 200)])),
            Some(plan(&[(0, 250), (1, 150)])),
        );
        let coverage = build_coverage(&out, 2, Some("2026-06-01".into()), 0);
        assert_eq!(coverage.status(), COVERAGE_COMPARABLE);
        assert_eq!(coverage.full_basket_saving_cents, Some(100));
        assert_eq!(coverage.common_scope, vec![0, 1]);
        assert_eq!(coverage.covered_subtotal_cents, Some(400));
    }

    #[test]
    fn unequal_coverage_suppresses_the_full_basket_and_bounds_the_subtotal() {
        let out = output(
            Some(plan(&[(0, 300), (1, 200)])),
            Some(plan(&[(0, 250), (1, 150), (2, 100)])),
        );
        let coverage = build_coverage(&out, 3, Some("2026-06-01".into()), 0);
        assert_eq!(coverage.status(), COVERAGE_UNEQUAL);
        assert_eq!(coverage.full_basket_saving_cents, None);
        assert_eq!(coverage.common_scope, vec![0, 1]);
        assert_eq!(coverage.recommended_only, vec![2]);
        // Bounded estimate over the two common lines only.
        assert_eq!(coverage.covered_subtotal_cents, Some(400));
        assert_eq!(coverage.covered_saving_cents, Some(100));
    }

    #[test]
    fn no_plan_reports_no_plan_status() {
        let out = output(None, None);
        let coverage = build_coverage(&out, 4, None, 0);
        assert_eq!(coverage.status(), COVERAGE_NO_PLAN);
        assert_eq!(coverage.full_basket_saving_cents, None);
        assert_eq!(coverage.covered_subtotal_cents, None);
    }

    #[test]
    fn label_names_coverage_and_date() {
        assert_eq!(
            covered_subtotal_label(8, 10, Some("2026-06-01"), 0),
            "Covered subtotal: 8 of 10 lines, prices as of 2026-06-01"
        );
        assert_eq!(
            covered_subtotal_label(8, 10, None, 0),
            "Covered subtotal: 8 of 10 lines"
        );
        assert_eq!(
            covered_subtotal_label(8, 10, Some("2026-06-01"), 1),
            "Covered subtotal: 8 of 10 lines, prices as of 2026-06-01, 1 offer undated"
        );
        assert_eq!(
            covered_subtotal_label(8, 10, None, 2),
            "Covered subtotal: 8 of 10 lines, prices as of unknown, 2 offers undated"
        );
    }

    #[test]
    fn oldest_observed_date_picks_the_minimum_and_counts_nulls() {
        let offers = vec![
            OfferEvidence {
                offer_id: "a".into(),
                retailer: Retailer::Tesco,
                retailer_product_id: "p".into(),
                source: Some("basketwatch".into()),
                product_name: "p".into(),
                observed_date: Some("2026-06-03".into()),
                shelf_price_cents: 1,
                loyalty_price_cents: None,
                loyalty_scheme: None,
                unit_price_eur_micros: None,
                unit_price_basis: None,
                pack_size_text: None,
                size_value: None,
                size_unit: None,
                pack_count: None,
                usable_base_units: None,
                previous_price_cents: None,
                offer_text: None,
                offer_valid_until: None,
            },
            OfferEvidence {
                offer_id: "b".into(),
                retailer: Retailer::Aldi,
                retailer_product_id: "q".into(),
                source: Some("manual".into()),
                product_name: "q".into(),
                observed_date: None,
                shelf_price_cents: 1,
                loyalty_price_cents: None,
                loyalty_scheme: None,
                unit_price_eur_micros: None,
                unit_price_basis: None,
                pack_size_text: None,
                size_value: None,
                size_unit: None,
                pack_count: None,
                usable_base_units: None,
                previous_price_cents: None,
                offer_text: None,
                offer_valid_until: None,
            },
            OfferEvidence {
                offer_id: "c".into(),
                retailer: Retailer::Dunnes,
                retailer_product_id: "r".into(),
                source: None,
                product_name: "r".into(),
                observed_date: Some("2026-05-30".into()),
                shelf_price_cents: 1,
                loyalty_price_cents: None,
                loyalty_scheme: None,
                unit_price_eur_micros: None,
                unit_price_basis: None,
                pack_size_text: None,
                size_value: None,
                size_unit: None,
                pack_count: None,
                usable_base_units: None,
                previous_price_cents: None,
                offer_text: None,
                offer_valid_until: None,
            },
        ];
        assert_eq!(oldest_observed_date(&offers).as_deref(), Some("2026-05-30"));
        assert_eq!(undated_offer_count(&offers), 1);
    }

    #[test]
    fn used_offer_ids_unions_best_single_and_recommended() {
        let out = output(Some(plan(&[(0, 300)])), Some(plan(&[(0, 250), (1, 150)])));
        let ids = used_offer_ids(&out);
        assert_eq!(
            ids.into_iter().collect::<Vec<_>>(),
            vec!["of-0".to_string(), "of-1".to_string()]
        );
    }

    #[test]
    fn digest_is_stable_and_payload_sensitive() {
        let a = snapshot_digest("{}", "[]", "{}");
        assert_eq!(a, snapshot_digest("{}", "[]", "{}"));
        assert_ne!(a, snapshot_digest("{}", "[]", "{\"x\":1}"));
    }
}
