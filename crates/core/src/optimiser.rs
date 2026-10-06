//! Basket optimiser: cheapest single-store and two-store shopping plans.
//!
//! Given a basket of [`BasketItem`]s (each already resolved to a canonical item
//! and carrying its constraints) the optimiser produces four plans
//! (`docs/design.md` §10):
//!
//! 1. [`OptimiserOutput::best_single`] — the cheapest *complete-as-possible*
//!    basket at a single retailer.
//! 2. [`OptimiserOutput::best_two_store`] — the cheapest split across exactly
//!    two retailers.
//! 3. [`OptimiserOutput::theoretical_minimum`] — every item at its globally
//!    cheapest retailer, ignoring store count (a benchmark, never a routed plan).
//! 4. [`OptimiserOutput::recommended`] — the practical recommendation, which
//!    applies a convenience threshold to decide whether a two-store split is
//!    worth the extra trip.
//!
//! # The fulfilment rule (why missing-count beats cost)
//!
//! Plans are ranked by `(missing_count ASC, total_cost ASC)`: **fewer missing
//! items always beats a lower price.** A plan that prices every item, even if it
//! is more expensive, is preferred over a cheaper plan that simply omits the
//! items it cannot source. A household would rather pay a little more and get
//! everything than save money and arrive home missing things. This ordering is
//! applied uniformly — single-store selection, two-store pairing, and the final
//! recommendation — so cost can never override completeness anywhere.
//!
//! # Pre-step (caller responsibility)
//!
//! The caller builds [`OptimiserInput`] *before* the optimiser runs:
//! parse count/size from raw text, load each canonical item's
//! `default_constraint_policy`, load its `item_constraints` row, and merge any
//! requested size from the parsed input into that constraints object (so
//! `"Fairy 500ml"` keeps both `brand='Fairy'` and a preferred size). The
//! optimiser receives the result and does not touch raw text again.
//!
//! # Money
//!
//! All money is integer [`Cents`] (`crate::domain`). Nothing here uses `f64` for
//! a price. Size comparison (the ±20% band) is the only place `f64` appears, and
//! only for the dimensionless size values that already live as `REAL` columns.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::db::{self, DbError};
use crate::domain::{
    Cents, ConstraintPolicy, FreshnessConfidence, ItemConstraints, MatchStatus, ProductMatch,
    ProductOffer, Retailer, RetailerProduct, SizeBasis, StoreStatus, SubstitutionPolicy,
};
use crate::normalise::normalize;
use sqlx::sqlite::SqlitePool;

/// Default second-store convenience threshold: a two-store split must save more
/// than this over the best single store to be recommended (design §10: €10–€15).
///
/// The issue's acceptance criterion pins the lower bound: a saving of *less than*
/// €10 must leave `clears_convenience_threshold = false`, so the default is
/// exactly €10 = 1000 cents. Callers may override it per run.
pub const DEFAULT_THRESHOLD_2_CENTS: Cents = 1000;

/// Default third-store convenience threshold (cents): €25.00.
///
/// Phase-0/Phase-4 plans are single- and two-store only, so threshold-3 never
/// changes a plan today; it is accepted, validated, and persisted for
/// forward-compatibility. €25 is the documented default
/// (`docs/implementation-plan.md`, P4-T2: "€25 (2500 cents) for threshold_3").
/// Lives here, alongside [`DEFAULT_THRESHOLD_2_CENTS`], so both the settings
/// endpoint and the optimise endpoint import the pair from one canonical place.
pub const DEFAULT_THRESHOLD_3_CENTS: Cents = 2500;

/// The default `(threshold_2, threshold_3)` pair in cents (€10 / €25), used when
/// a household has no stored settings row. Centralised so every caller agrees.
pub const DEFAULT_THRESHOLDS: (Cents, Cents) =
    (DEFAULT_THRESHOLD_2_CENTS, DEFAULT_THRESHOLD_3_CENTS);

/// Second-store convenience threshold (cents) applied when the two-store split
/// includes a retailer the household marked as **nearby** (issue #98). A nearby
/// store is cheap to reach (e.g. SuperValu around the corner), so the extra trip
/// is worth a smaller saving than the default €10–€15 — €5.00.
///
/// The nearby threshold is applied as `min(threshold_2_cents, NEARBY_THRESHOLD_2_CENTS)`
/// so a household that deliberately sets an even smaller threshold keeps it.
pub const NEARBY_THRESHOLD_2_CENTS: Cents = 500;

/// Tolerance band for size matching: a product's size must be within ±20% of the
/// constrained size (and on the same [`SizeBasis`]) to qualify. Phase-0 scope
/// (the issue): size and brand are the only enforced constraints.
const SIZE_TOLERANCE: f64 = 0.20;

/// Inclusive lower bound (cents) of the Dunnes voucher window: a Dunnes basket in
/// `[DUNNES_VOUCHER_LOW, DUNNES_VOUCHER_HIGH]` is flagged as near the €50
/// spend-and-save threshold (design §10). €45.00.
const DUNNES_VOUCHER_LOW: Cents = 4500;
/// Inclusive upper bound (cents) of the Dunnes voucher window. €50.00.
const DUNNES_VOUCHER_HIGH: Cents = 5000;

// ---------------------------------------------------------------------------
// Input types (caller-constructed)
// ---------------------------------------------------------------------------

/// One line of a basket, resolved and ready to price.
///
/// The caller is responsible for the pre-step: `canonical_item_id` is `None`
/// only when resolution failed (the item is then *unresolved* and excluded from
/// every total); `constraints` already has any requested size merged in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasketItem {
    /// Caller-supplied identity for the source line this item came from (e.g. a
    /// `list_entries.id`), echoed back on every output item so the caller can map
    /// a priced or excluded line back to its origin. `None` when the caller has no
    /// persisted line (e.g. the CLI costing fixture text). Opaque to the
    /// optimiser; it never affects pricing or ranking.
    pub entry_id: Option<String>,
    /// The original display text for this line (surfaced back in the output so
    /// the household recognises the item).
    pub display_text: String,
    /// Resolved canonical item id, or `None` when the line could not be
    /// resolved. `None` ⇒ the item goes to [`OptimiserOutput::unresolved_items`].
    pub canonical_item_id: Option<String>,
    /// How many packs to buy. `line_total = selected_price × count`.
    pub count: u32,
    /// Constraint strictness for this item (from the canonical item's
    /// `default_constraint_policy`).
    pub constraint_policy: ConstraintPolicy,
    /// The merged constraints object (brand, preferred size, …) or `None` when
    /// the item has no `item_constraints` row.
    pub constraints: Option<ItemConstraints>,
    /// The resolved canonical item's category id (issue #99), used to apply the
    /// household's per-store category rules. `None` when the item is unresolved
    /// or its category is unknown, in which case no category rule can exclude it
    /// (an item we cannot classify is never silently dropped).
    #[serde(default)]
    pub category_id: Option<String>,
    /// The resolved canonical item's sub-category id (issue #101), when the
    /// category is split. A sub-category rule refines the category rule
    /// (issue #99). `None` when the item is unresolved or not split.
    #[serde(default)]
    pub subcategory_id: Option<String>,
    /// Whether the item's category is fresh food (issue #100, `categories.is_fresh`):
    /// produce, meat/fish, dairy, bakery. Drives only the packaged/fresh split of
    /// the Aldi comparison; it never affects plan ranking or routing. `false` for
    /// an unresolved item or one whose category is unknown/not marked fresh.
    #[serde(default)]
    pub is_fresh: bool,
}

/// Identity and input order for one basket item, echoed in [`OptimiserOutput`]
/// so callers can account for entries omitted by a particular store plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BasketItemIdentity {
    /// Index into `OptimiserInput::items`.
    pub item_index: usize,
    /// Caller-supplied identity for the source line, when one exists.
    pub entry_id: Option<String>,
    /// Original display text.
    pub display_text: String,
}

/// One per-store category rule (issue #99): whether `retailer` may fulfil items
/// in `category_id` — or, when `subcategory_id` is set, only that sub-category
/// of it (a refinement over the whole-category rule).
///
/// Rules are matched most-specific-first against an item's category and
/// sub-category; an item with no matching rule is allowed. The list on a
/// [`StoreSetting`] is the household's explicit rules for that store (empty ⇒
/// every category allowed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryRule {
    /// The category this rule targets (a `categories.id`).
    pub category_id: String,
    /// When `Some`, the rule applies only to this sub-category of `category_id`
    /// (a `subcategories.id`); when `None`, to the whole category.
    #[serde(default)]
    pub subcategory_id: Option<String>,
    /// Whether the store may fulfil items in this scope.
    pub allowed: bool,
}

/// One household's stored disposition of a retailer (issue #98): whether it takes
/// part in plan generation and whether its trip is cheap enough to lower the
/// second-store convenience threshold. Carried on [`OptimiserInput`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreSetting {
    /// The retailer this setting applies to.
    pub retailer: Retailer,
    /// Whether the store is active, occasional (trip-gated) or off.
    pub status: StoreStatus,
    /// True when the store is close by, so a two-store split including it clears
    /// the lower [`NEARBY_THRESHOLD_2_CENTS`] bar.
    pub nearby: bool,
    /// Per-category / sub-category rules (issue #99). Empty ⇒ every category is
    /// allowed (the pre-#99 behaviour). See [`CategoryRule`].
    #[serde(default)]
    pub category_rules: Vec<CategoryRule>,
}

/// Everything the optimiser needs for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptimiserInput {
    /// The household whose matches and constraints are in play.
    pub household_id: String,
    /// The current run number for freshness comparison. `None` (no successful
    /// run yet) makes every freshness confidence [`FreshnessConfidence::Unknown`].
    pub current_source_run_number: Option<i64>,
    /// Second-store convenience threshold in cents (see
    /// [`DEFAULT_THRESHOLD_2_CENTS`]).
    pub threshold_2_cents: Cents,
    /// The basket lines.
    pub items: Vec<BasketItem>,
    /// Per-retailer household settings (issue #98). Empty ⇒ every retailer is
    /// [`StoreStatus::Active`] and none is nearby — the pre-#98 behaviour.
    pub store_settings: Vec<StoreSetting>,
    /// Retailers the household is visiting today, which admit their
    /// [`StoreStatus::Occasional`] stores into this run's plans. Empty by default.
    pub trip_retailers: Vec<Retailer>,
}

impl OptimiserInput {
    /// Construct an input with the default convenience threshold
    /// ([`DEFAULT_THRESHOLD_2_CENTS`]) and no store preferences (every retailer
    /// active, none nearby, no trip) — i.e. pre-#98 behaviour.
    pub fn new(
        household_id: impl Into<String>,
        current_source_run_number: Option<i64>,
        items: Vec<BasketItem>,
    ) -> Self {
        Self {
            household_id: household_id.into(),
            current_source_run_number,
            threshold_2_cents: DEFAULT_THRESHOLD_2_CENTS,
            items,
            store_settings: Vec::new(),
            trip_retailers: Vec::new(),
        }
    }

    /// The stored status for `retailer`, or [`StoreStatus::Active`] when the
    /// household has no setting for it (the default that preserves pre-#98
    /// behaviour).
    fn store_status(&self, retailer: Retailer) -> StoreStatus {
        self.store_settings
            .iter()
            .find(|s| s.retailer == retailer)
            .map(|s| s.status)
            .unwrap_or(StoreStatus::Active)
    }

    /// Whether `retailer` is eligible for this run's plans: an active store always
    /// is; an occasional store is eligible only when listed in [`Self::trip_retailers`];
    /// an off store never is.
    fn is_eligible(&self, retailer: Retailer) -> bool {
        match self.store_status(retailer) {
            StoreStatus::Active => true,
            StoreStatus::Occasional => self.trip_retailers.contains(&retailer),
            StoreStatus::Off => false,
        }
    }

    /// Whether the household marked `retailer` as nearby.
    fn is_nearby(&self, retailer: Retailer) -> bool {
        self.store_settings
            .iter()
            .any(|s| s.retailer == retailer && s.nearby)
    }

    /// Whether `retailer` may fulfil the basket item at `item_index` under the
    /// household's per-store category rules (issue #99).
    ///
    /// Default-allow: an item with no category, a store with no rules, or an item
    /// with no matching rule is allowed. A rule for the item's sub-category beats
    /// one for its category (the refinement), and that rule's `allowed` decides.
    fn store_allows_item(&self, retailer: Retailer, item_index: usize) -> bool {
        let Some(item) = self.items.get(item_index) else {
            return true;
        };
        let Some(category_id) = item.category_id.as_deref() else {
            return true;
        };
        let Some(setting) = self.store_settings.iter().find(|s| s.retailer == retailer) else {
            return true;
        };

        // Sub-category rule first: an explicit refinement always wins.
        if let Some(subcategory_id) = item.subcategory_id.as_deref() {
            if let Some(rule) = setting.category_rules.iter().find(|r| {
                r.category_id == category_id && r.subcategory_id.as_deref() == Some(subcategory_id)
            }) {
                return rule.allowed;
            }
        }
        // Otherwise the whole-category rule, if any.
        if let Some(rule) = setting
            .category_rules
            .iter()
            .find(|r| r.category_id == category_id && r.subcategory_id.is_none())
        {
            return rule.allowed;
        }
        true
    }

    /// Whether `retailer` is both eligible for this run and may fulfil the item
    /// at `item_index` under its category rules (issue #99). The single gate the
    /// plan builders apply.
    fn retailer_can_price_item(&self, retailer: Retailer, item_index: usize) -> bool {
        self.is_eligible(retailer) && self.store_allows_item(retailer, item_index)
    }
}

// ---------------------------------------------------------------------------
// Candidate data (the pure core's input)
// ---------------------------------------------------------------------------

/// A priced, constraint-passing option for one basket item at one retailer.
///
/// Produced by `db.rs` reads (or hand-built in tests) and consumed by the pure
/// [`optimise_core`]. One basket item can have several candidates across (and
/// even within) retailers; the optimiser picks the cheapest per (item, retailer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Index of the basket item this candidate prices (into `OptimiserInput::items`).
    pub item_index: usize,
    /// The retailer this option is sold at.
    pub retailer: Retailer,
    /// The matched retailer product.
    pub product: RetailerProduct,
    /// The latest offer for that product.
    pub offer: ProductOffer,
    /// Whether the underlying match was `accepted` (vs `suggested`). Used only as
    /// a tie-breaker when prices are equal.
    pub accepted: bool,
}

impl Candidate {
    /// The effective per-unit price: the loyalty price when present, else the
    /// shelf price. Loyalty schemes (Clubcard, Real Rewards) are assumed active
    /// (design §10) so their price is the price the household actually pays.
    pub fn effective_unit_price(&self) -> Cents {
        self.offer
            .loyalty_price_cents
            .unwrap_or(self.offer.price_cents)
    }

    /// Whether a loyalty price was applied (the loyalty price was present and so
    /// became the effective price). Surfaced on the plan line for transparency.
    pub fn loyalty_applied(&self) -> bool {
        self.offer.loyalty_price_cents.is_some()
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// One priced line of a [`StorePlan`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanLine {
    /// Index of the basket item (into `OptimiserInput::items`).
    pub item_index: usize,
    /// The source list entry this line fulfils, echoed from the [`BasketItem`]
    /// (`None` for callers without a persisted line). Lets a per-store view map
    /// the priced line back to the household's list entry.
    pub entry_id: Option<String>,
    /// The item's display text (copied for convenience).
    pub display_text: String,
    /// The chosen retailer product's display name (e.g. "Tesco Whole Milk 2L"),
    /// so the household can see which product was matched on the plan line.
    pub product_name: String,
    /// The resolved canonical item id this line prices. Always `Some` for a
    /// priced line (an unpriced/unresolved item never produces a `PlanLine`), so
    /// the UI can pair it with `retailer_product_id` to accept/reject the
    /// underlying product match (P5-T1, `POST /api/match-feedback`).
    pub canonical_item_id: Option<String>,
    /// The retailer this line is fulfilled at.
    pub retailer: Retailer,
    /// The chosen retailer product id.
    pub retailer_product_id: String,
    /// The chosen offer id.
    pub offer_id: String,
    /// The effective per-unit price actually charged (loyalty when applicable).
    pub unit_price_cents: Cents,
    /// `unit_price_cents × count`.
    pub line_total_cents: Cents,
    /// Whether a loyalty price was applied on this line.
    pub loyalty_applied: bool,
    /// Freshness confidence for the underlying offer.
    pub freshness_confidence: FreshnessConfidence,
}

/// A concrete shopping plan over one or more stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorePlan {
    /// The distinct retailers in this plan, in fulfilment order. Exactly one for
    /// [`OptimiserOutput::best_single`], exactly two for
    /// [`OptimiserOutput::best_two_store`].
    pub stores: Vec<Retailer>,
    /// Priced lines, one per item that could be fulfilled by this plan.
    pub lines: Vec<PlanLine>,
    /// Sum of `line_total_cents` across `lines`. Excludes missing items.
    pub total_cents: Cents,
    /// How many basket items this plan could *not* fulfil. The primary ranking
    /// key (see the module-level fulfilment rule).
    ///
    /// This counts **every** unfulfilled resolved basket line, not just the ones
    /// this plan's store(s) failed to price:
    /// * items this plan's stores do not stock (plan-specific), plus
    /// * resolved items that produced no valid offer anywhere — the
    ///   [`OptimiserOutput::no_valid_offer_items`] (constant across all plans,
    ///   since they are unfulfillable at every store).
    ///
    /// Including the latter keeps the field honest: a plan never reports
    /// `missing_count == 0` while the basket still has resolved lines nobody can
    /// fulfil. Because that contribution is constant across plans it does not
    /// disturb relative ranking. Unresolved lines (no `canonical_item_id`) are
    /// *not* counted here — they are surfaced separately in
    /// [`OptimiserOutput::unresolved_items`] and excluded from every total.
    pub missing_count: usize,
    /// A Dunnes spend-and-save prompt, set when a Dunnes line of this plan puts
    /// the Dunnes spend in the €45–€50 voucher window.
    pub dunnes_voucher_offer_text: Option<String>,
}

/// True when two priced-line sets cover exactly the same basket item indices, so
/// their totals are like-for-like and a saving may be claimed between them.
///
/// This is the one comparison the product uses to *refuse* a cross-plan cost
/// comparison: the optimise explanation suppresses its saving sentence when the
/// recommended plan prices a different set of items than the best single store
/// ([`crate::optimiser`] consumers in the API), and the savings ledger refuses to
/// record a gross estimate as a saving on the same rule. A set that is missing a
/// line, has an extra one, or substitutes a different line is not comparable —
/// its total difference is not a saving.
///
/// Quantities are not part of a [`PlanLine`], so this compares lines only; pack
/// sizes beyond the planned amount are out of scope for this comparison.
pub fn prices_same_lines<'a>(
    a: impl IntoIterator<Item = &'a usize>,
    b: impl IntoIterator<Item = &'a usize>,
) -> bool {
    let a: BTreeSet<usize> = a.into_iter().copied().collect();
    let b: BTreeSet<usize> = b.into_iter().copied().collect();
    a == b
}

/// Which plan the recommendation chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedKind {
    /// The best single-store plan was recommended.
    SingleStore,
    /// The best two-store split was recommended.
    TwoStore,
}

/// The practical recommendation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    /// Which plan kind was chosen.
    pub kind: RecommendedKind,
    /// The chosen plan.
    pub plan: StorePlan,
    /// Whether the two-store saving cleared the convenience threshold. `false`
    /// when the saving was too small (or completeness, not cost, drove the
    /// choice — a less-complete cheaper split never wins).
    pub clears_convenience_threshold: bool,
    /// Saving of the two-store split over the best single store, in cents. May be
    /// negative or zero (then the single store is recommended). Only meaningful
    /// when both plans are equally complete.
    pub two_store_saving_cents: Cents,
}

/// What the basket would have cost at Aldi, split fresh versus packaged, compared
/// with the recommended plan (issue #100).
///
/// This is evidence, never a routed plan: it is computed from Aldi's own
/// constraint-passing offers (ignoring the household's store settings and
/// per-store category rules) and the recommended plan's lines, and it never
/// affects which plan is recommended. The household collects it so it can judge
/// whether Aldi is worth shopping at — for packaged food today, and fresh food
/// only if the quality concern turns out not to be worth the price gap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AldiComparison {
    /// The packaged (non-fresh) items.
    pub packaged: AldiComparisonSplit,
    /// The fresh-food items (produce, meat/fish, dairy, bakery).
    pub fresh: AldiComparisonSplit,
}

/// One half of an [`AldiComparison`]: Aldi's cost versus the recommended plan's
/// cost for the same items.
///
/// Both totals cover exactly the items that Aldi prices *and* the recommended
/// plan prices, so the comparison is like-for-like. An item only one side prices
/// is excluded from both totals rather than compared against nothing;
/// [`Self::item_count`] carries how many items the figures cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AldiComparisonSplit {
    /// How many items both Aldi and the recommended plan price (the covered set).
    pub item_count: usize,
    /// Aldi's total for those items (effective unit price × count).
    pub aldi_total_cents: Cents,
    /// The recommended plan's total for the same items.
    pub recommended_total_cents: Cents,
    /// `recommended_total_cents − aldi_total_cents`: positive means Aldi would
    /// have been cheaper for these items, negative means it would have cost more.
    pub saving_cents: Cents,
}

/// An item that could not be resolved to a canonical item (no `canonical_item_id`).
/// Excluded from every total; surfaced so the household can clarify it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedItem {
    /// Index into `OptimiserInput::items`.
    pub item_index: usize,
    /// The source list entry, echoed from the [`BasketItem`] (`None` for callers
    /// without a persisted line). Lets the UI keep unallocated items attached to
    /// their household entry.
    pub entry_id: Option<String>,
    /// The original display text.
    pub display_text: String,
}

/// Why an otherwise-resolved item produced no priceable offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoOfferReason {
    /// The item resolved but has no `accepted`/`suggested` matches with offers.
    NoMatches,
    /// Every candidate was filtered out by the item's constraints.
    ConstraintsEliminatedAll,
    /// Every candidate was at a store excluded by the item's category rules.
    StoreCategoryRulesExcludedAll,
    /// `policy = 'brand'` but no brand is configured. A configuration error: we
    /// never silently fall back to `any`, so the item is rejected with a warning.
    BrandPolicyMissingBrand,
    /// `substitution_policy = 'strict'` and the exact (accepted) matched product
    /// has no current, constraint-passing offer. A substitute exists but Strict
    /// forbids offering it, so the item is shown as missing rather than swapped
    /// for an alternative (Backlog-T1). Distinct from
    /// [`NoOfferReason::ConstraintsEliminatedAll`]: the item is missing by
    /// *policy*, not because every candidate failed a field filter.
    ///
    /// Chosen semantic ("exact match"): under Strict, an *exact* offer is any
    /// candidate whose underlying match status is `Accepted`. A merely
    /// `Suggested` match is **not** exact, so an item with only suggested matches
    /// (and no accepted one) is reported `StrictNoExactOffer` even though those
    /// suggested candidates priced fine — Strict will not offer an unconfirmed
    /// substitute. This mirrors [`SubstitutionPolicy::Strict`] in the filter.
    StrictNoExactOffer,
}

/// An item that resolved but yielded no valid offer. Carries a structured reason
/// (including the brand-config-error case) so the caller can warn appropriately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoValidOfferItem {
    /// Index into `OptimiserInput::items`.
    pub item_index: usize,
    /// The source list entry, echoed from the [`BasketItem`] (`None` for callers
    /// without a persisted line).
    pub entry_id: Option<String>,
    /// The original display text.
    pub display_text: String,
    /// The resolved canonical item id. Always present: only a resolved entry
    /// reaches `no_valid_offer_items` (an unresolved entry is excluded from
    /// pricing and surfaced in `unresolved_items` instead). Lets the UI link the
    /// warning straight to that item's preferences screen.
    pub canonical_item_id: String,
    /// Structured reason for the warning.
    pub reason: NoOfferReason,
}

/// One constraint-passing candidate offer for an ask-gated item, carried on
/// [`NeedsConfirmationItem`] so the UI can render a chooser (issue #78) instead
/// of just a count. The fields are the minimum a household needs to pick:
/// product identity (id/name/brand/size), price, and where it is sold.
///
/// `price_cents` is the **effective** per-unit price (loyalty when present, else
/// shelf), matching [`Candidate::effective_unit_price`] so the chooser shows the
/// price the household would actually pay per pack. This is the candidate's
/// [`PlanLine::unit_price_cents`], not its `line_total_cents`: once the candidate
/// is confirmed, the plan line renders `price_cents × count`, so for an item with
/// `count > 1` the line total is a multiple of this number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateOffer {
    /// The retailer product id (paired with the item's `canonical_item_id` to
    /// confirm the choice via `POST /api/match-feedback` with action `prefer`).
    pub retailer_product_id: String,
    /// The retailer product name.
    pub name: String,
    /// The product brand, when known.
    pub brand: Option<String>,
    /// Human-readable pack/size text (e.g. "500ml"), when known.
    pub size_text: Option<String>,
    /// Effective per-unit price in cents (loyalty when applicable, else shelf).
    pub price_cents: Cents,
    /// The retailer this candidate is sold at.
    pub retailer: Retailer,
}

/// A resolved item whose `substitution_policy = 'ask'` produced at least one
/// qualifying offer, but which must be **confirmed by the household** before it
/// is auto-selected (Backlog-T1).
///
/// Surfaced separately from priced plan lines and from
/// [`NoValidOfferItem`]: the item is *priceable* (a candidate exists) but the
/// optimiser deliberately does not commit it to a plan. Callers render it as a
/// confirmation prompt. The carried `candidate_count` lets the UI say how many
/// options await a choice, while `candidates` (issue #78) carries the actual
/// offers so the UI can render a chooser and submit a `prefer`; confirming one
/// brings the item into the plans on the next optimise. `candidate_count` always
/// equals `candidates.len()` (retained for back-compat with existing callers).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeedsConfirmationItem {
    /// Index into `OptimiserInput::items`.
    pub item_index: usize,
    /// The source list entry, echoed from the [`BasketItem`] (`None` for callers
    /// without a persisted line).
    pub entry_id: Option<String>,
    /// The original display text.
    pub display_text: String,
    /// The resolved canonical item id, so the UI can submit a `prefer` choice
    /// (`POST /api/match-feedback` needs `canonical_item_id` + the chosen
    /// `retailer_product_id`). Always present: an ask item is, by definition,
    /// resolved (`classify_items` only reaches the policy branch for a resolved,
    /// priceable item).
    pub canonical_item_id: String,
    /// How many constraint-passing candidate offers were found for the item.
    /// Equal to `candidates.len()`.
    pub candidate_count: usize,
    /// The constraint-passing candidate offers the household chooses between
    /// (issue #78). Cheapest-effective-price first, then by retailer then product
    /// id, so the ordering is deterministic.
    pub candidates: Vec<CandidateOffer>,
}

/// Which published plan a [`PlanReason`] belongs to. The serde names match the
/// plan fields on [`OptimiserOutput`] so a caller can group reasons by the card
/// it renders (issue #103).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    /// [`OptimiserOutput::recommended`].
    Recommended,
    /// [`OptimiserOutput::best_single`].
    BestSingle,
    /// [`OptimiserOutput::best_two_store`].
    BestTwoStore,
    /// [`OptimiserOutput::theoretical_minimum`] (a benchmark, never routed).
    TheoreticalMinimum,
}

/// Stable machine-readable code for a [`PlanReason`].
///
/// The code carries the *meaning*; the human sentence is rendered by the caller
/// (category names and euro formatting are presentation concerns the pure core
/// does not own). Callers must tolerate codes they do not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    /// `item_indices` are priced at `retailers[0]`, the cheapest *eligible* store
    /// for each of them (the store the theoretical minimum would choose).
    CheapestAtStore,
    /// `item_indices` are priced only at `retailers[0]`; no other eligible store
    /// has a qualifying price on record for them. Absence of a price elsewhere is
    /// not proof the item is out of stock — only that we have no known price.
    OnlyStoreStocking,
    /// `item_indices` carry a brand constraint (policy or brand lists), so the
    /// store's qualifying product is the one the household's brand rule allows.
    BrandRule,
    /// Plan-wide coverage: `item_indices` are the lines this plan prices and
    /// `cents` is its total. Always present when the plan has lines, so a subtotal
    /// never looks like a complete basket.
    Coverage,
    /// `item_indices` have a known price elsewhere but none at this plan's stores.
    /// This is *no known price here*, not proof of unavailability in-store.
    NoKnownPriceHere,
    /// `item_indices` resolved but no eligible store yielded a qualifying offer.
    NoKnownPriceAnywhere,
    /// `item_indices` could not be resolved to a catalogue item at all.
    UnresolvedItems,
    /// `item_indices` are lines at `retailers` whose price is not from the latest
    /// successful source run.
    StalePrices,
    /// `snapshot_date` is the oldest known source snapshot date among this
    /// plan's offers. `item_indices` lists plan lines with no recorded date.
    PriceAsOf,
    /// The recommended two-store split's saving (`cents`) clears the convenience
    /// bar (`threshold_cents`) for the store(s) it adds in `retailers`.
    ThresholdCleared,
    /// The best split would have saved `cents`, below the `threshold_cents` bar,
    /// so the single store was kept. `retailers` are the stores it would add.
    ThresholdNotCleared,
    /// The best split would drop `item_indices`, so the more complete single
    /// store was kept. `retailers` are the stores it would add.
    SplitDropsItems,
    /// The recommended split reaches `item_indices` the best single store cannot,
    /// which is why the extra stop is worth it even below the saving bar.
    MoreComplete,
    /// A whole-store alternative `retailers[0]` was considered; `cents` is
    /// `plan.total − that_store_alone.total` (positive: the store alone is
    /// cheaper; negative: it would cost that much more). Only emitted when the two
    /// plans price the same coverage, so the figures are like-for-like.
    RejectedStore,
    /// `retailers[0]` alone would price a different set of items, so its total
    /// is not comparable with this plan's; no saving is reported.
    CoverageDiffers,
    /// `retailers[0]` is switched off in the household's store settings, so it is
    /// never planned for.
    StoreOff,
    /// `retailers[0]` is an occasional store not marked for today's trip, so it is
    /// left out of this run.
    StoreNotOnTrip,
    /// `retailers[0]`'s per-store category rules exclude `item_indices`, so it may
    /// not fulfil them even though it stocks them (issue #99).
    CategoryExcluded,
}

/// One deterministic, plan-specific reason explaining a plan (issue #103).
///
/// A reason is a structured fact, never free text: it names the basket items and
/// retailers it covers, the money it quantifies, the convenience bar it compares
/// against, and the price date it refers to. It is derived purely from the plan's
/// actual lines plus the run's constraints, threshold and known price dates, so
/// it is reproducible and traceable (no LLM, no randomness).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanReason {
    /// The plan card this reason explains.
    pub plan: PlanKind,
    /// What the reason means (see [`ReasonCode`]).
    pub code: ReasonCode,
    /// Basket item indices this reason covers (empty = plan-wide).
    pub item_indices: Vec<usize>,
    /// Retailers this reason refers to; semantics depend on the code.
    pub retailers: Vec<Retailer>,
    /// Money this reason quantifies (saving, overspend, or plan total), in cents.
    pub cents: Option<Cents>,
    /// The convenience bar this reason compares against, in cents, when relevant.
    pub threshold_cents: Option<Cents>,
    /// ISO-8601 source snapshot date this reason refers to, when relevant.
    pub snapshot_date: Option<String>,
}

/// The full optimiser result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptimiserOutput {
    /// Identities of every input item, in input order, so callers can account
    /// for entries omitted by a particular plan.
    pub basket_items: Vec<BasketItemIdentity>,
    /// Cheapest plan at a single retailer (exactly one store), or `None` if no
    /// item could be priced anywhere.
    pub best_single: Option<StorePlan>,
    /// Cheapest plan across exactly two retailers, or `None` if fewer than two
    /// retailers can contribute (then `best_single` is the only multi-store plan).
    pub best_two_store: Option<StorePlan>,
    /// Every item at its globally cheapest retailer (benchmark only), or `None`
    /// if nothing could be priced.
    pub theoretical_minimum: Option<StorePlan>,
    /// The practical recommendation, or `None` if no plan exists.
    pub recommended: Option<Recommendation>,
    /// Items with no `canonical_item_id`, excluded from all totals.
    pub unresolved_items: Vec<UnresolvedItem>,
    /// Resolved items that nonetheless produced no valid offer.
    pub no_valid_offer_items: Vec<NoValidOfferItem>,
    /// Resolved, priceable items held back for household confirmation because
    /// their `substitution_policy = 'ask'` (Backlog-T1). Excluded from every
    /// plan and from all totals until the household chooses.
    pub needs_confirmation_items: Vec<NeedsConfirmationItem>,
    /// Deterministic, plan-specific reasons for all four published plans (issue
    /// #103), ordered most-important-first within each [`PlanKind`]. Callers
    /// render up to four per plan card.
    pub reasons: Vec<PlanReason>,
    /// What Aldi would have cost the basket versus the recommended plan, split
    /// packaged versus fresh (issue #100). `None` when there is no recommended
    /// plan to compare against. Computed independently of the store-off and
    /// category-exclusion rules and never used to rank or route plans; a split
    /// with `item_count == 0` means Aldi has no qualifying price to compare.
    pub aldi_comparison: Option<AldiComparison>,
}

// ---------------------------------------------------------------------------
// Constraint filtering
// ---------------------------------------------------------------------------

/// Outcome of applying an item's constraints to one candidate product.
enum ConstraintCheck {
    /// The product passes; it may be priced.
    Pass,
    /// The product fails a constraint (brand mismatch, size out of band); skip it.
    Reject,
    /// The item's *configuration* is invalid (policy=brand with no brand). The
    /// whole item is rejected to `no_valid_offer_items`, never silently relaxed.
    ConfigError,
}

/// Apply an item's constraints to a single product (Backlog-T1: full
/// enforcement).
///
/// The brand check runs only under `policy = 'brand'` (and is a config error if
/// the policy is set but no brand is configured). Every other field filter —
/// `allowed_brands`, `excluded_brands`, `pack_count`, `form_factor`,
/// `dietary_flags`, and the preferred-size band — is enforced whenever the
/// corresponding constraint is present, independent of `policy`. Brand
/// comparisons (the `brand` policy plus the allow/block lists) are normalised
/// via [`normalize`] so casing/punctuation differences do not leak products
/// through.
///
/// Substitution policy (`Strict`/`Ask`) is **not** decided here: it is a
/// per-item concern handled in [`classify_items`] after the per-product filter
/// has produced the surviving candidates.
fn check_constraints(
    policy: ConstraintPolicy,
    constraints: Option<&ItemConstraints>,
    product: &RetailerProduct,
) -> ConstraintCheck {
    // Brand enforcement: only under the `brand` policy.
    if policy == ConstraintPolicy::Brand {
        let Some(c) = constraints else {
            // policy=brand but no constraints row at all ⇒ no brand to match.
            return ConstraintCheck::ConfigError;
        };
        let Some(wanted_brand) = c.brand.as_deref() else {
            // policy=brand but brand IS NULL ⇒ configuration error. Never fall
            // back to `any`: a brand-locked item with no brand is a data bug, not
            // a licence to buy any brand.
            return ConstraintCheck::ConfigError;
        };
        match product.brand.as_deref() {
            Some(b) if normalize(b) == normalize(wanted_brand) => {}
            // Brand missing on the product, or a different brand: reject.
            _ => return ConstraintCheck::Reject,
        }
    }

    let Some(c) = constraints else {
        // No constraints row ⇒ nothing further to enforce.
        return ConstraintCheck::Pass;
    };

    // allowed_brands: if set & non-empty, the product's (normalised) brand must
    // appear in the allow-list. A product with no brand cannot be matched against
    // an allow-list, so it is rejected.
    if let Some(allowed) = c.allowed_brands.as_ref().filter(|l| !l.is_empty()) {
        let Some(brand) = product.brand.as_deref() else {
            return ConstraintCheck::Reject;
        };
        let norm = normalize(brand);
        if !allowed.iter().any(|a| normalize(a) == norm) {
            return ConstraintCheck::Reject;
        }
    }

    // excluded_brands: if the product's (normalised) brand is in the block-list,
    // reject. A product with no brand cannot be on the block-list, so it passes
    // this gate.
    if let Some(excluded) = c.excluded_brands.as_ref() {
        if let Some(brand) = product.brand.as_deref() {
            let norm = normalize(brand);
            if excluded.iter().any(|b| normalize(b) == norm) {
                return ConstraintCheck::Reject;
            }
        }
    }

    // pack_count: if set, the product's pack_count must match exactly. A product
    // with no pack_count cannot satisfy an explicit pack requirement.
    if let Some(wanted) = c.pack_count {
        if product.pack_count != Some(wanted) {
            return ConstraintCheck::Reject;
        }
    }

    // form_factor: if set, the product's form must match (case-insensitive). A
    // product with no form_factor cannot satisfy an explicit form requirement.
    if let Some(wanted) = c.form_factor.as_deref() {
        match product.form_factor.as_deref() {
            Some(f) if f.eq_ignore_ascii_case(wanted) => {}
            _ => return ConstraintCheck::Reject,
        }
    }

    // dietary_flags: if set & non-empty, the product's flag set must be a
    // SUPERSET — it must carry every required flag. A product with no flags
    // satisfies only an empty requirement. Comparison is case-insensitive on the
    // canonical lowercase tokens.
    if let Some(required) = c.dietary_flags.as_ref().filter(|l| !l.is_empty()) {
        let product_flags = product.dietary_flags.as_deref().unwrap_or(&[]);
        let has_all = required.iter().all(|req| {
            product_flags
                .iter()
                .any(|have| have.eq_ignore_ascii_case(req))
        });
        if !has_all {
            return ConstraintCheck::Reject;
        }
    }

    // Size band: enforced whenever a preferred size is set, regardless of policy.
    if let (Some(pref_value), Some(pref_basis)) = (c.preferred_size_value, c.preferred_size_basis) {
        if !size_within_band(product, pref_value, pref_basis) {
            return ConstraintCheck::Reject;
        }
    }

    ConstraintCheck::Pass
}

/// Whether `product`'s size is within ±[`SIZE_TOLERANCE`] of `pref_value` on the
/// same [`SizeBasis`].
///
/// A product with no size, or a size on a different basis, fails: we cannot
/// honour a size constraint we cannot compare. The product's `size_unit` is the
/// SI unit string (`"g"`, `"ml"`, `"each"`) written at ingest, parsed back into a
/// [`SizeBasis`]; an unparseable/absent unit fails closed.
fn size_within_band(product: &RetailerProduct, pref_value: f64, pref_basis: SizeBasis) -> bool {
    let Some(value) = product.size_value else {
        return false;
    };
    let Some(unit) = product.size_unit.as_deref() else {
        return false;
    };
    let Ok(basis) = SizeBasis::from_db_str(unit) else {
        return false;
    };
    if basis != pref_basis {
        return false;
    }
    let low = pref_value * (1.0 - SIZE_TOLERANCE);
    let high = pref_value * (1.0 + SIZE_TOLERANCE);
    value >= low && value <= high
}

// ---------------------------------------------------------------------------
// Pure optimiser core
// ---------------------------------------------------------------------------

/// The three computed plans before the recommendation is layered on.
#[derive(Debug, Clone, PartialEq)]
pub struct CorePlans {
    /// Best single-store plan, or `None` if nothing could be priced.
    pub best_single: Option<StorePlan>,
    /// Best two-store plan, or `None`.
    pub best_two_store: Option<StorePlan>,
    /// Theoretical minimum, or `None`.
    pub theoretical_minimum: Option<StorePlan>,
}

/// The pure core's full computation, retained so [`build_reasons`] can explain
/// the chosen plans from the same inputs without recomputing them.
struct CoreComputation {
    /// The three ranked plans.
    plans: CorePlans,
    /// Every genuine two-store candidate (spans exactly two stores), for the
    /// recommendation's threshold comparison.
    two_store_candidates: Vec<StorePlan>,
    /// Every eligible single-store plan, one per store, for the "rejected store"
    /// reasons (the best is `plans.best_single`).
    single_plans: Vec<StorePlan>,
    /// Cheapest candidate per (item, retailer) — the source of truth for a
    /// line's offer date and for which eligible stores price an item at all.
    best_per_item: BTreeMap<usize, HashMap<Retailer, Candidate>>,
    /// Basket item indices with at least one candidate anywhere.
    priced_items: Vec<usize>,
}

/// Run the optimiser over already-fetched, constraint-passing candidate data.
///
/// This is the pure, database-free core. [`optimise`] is the thin wrapper that
/// fetches candidates from the pool and calls this (via [`build_output`]).
///
/// `candidates` are the constraint-*passing* options for the resolved items.
/// Constraint filtering and the unresolved/no-offer classification happen in
/// [`classify_items`]; this core assumes every candidate it receives is valid.
///
/// `no_offer_count` is the number of resolved items that produced no valid offer
/// anywhere ([`OptimiserOutput::no_valid_offer_items`]). They are missing at
/// every store, so this constant is folded into each plan's `missing_count` to
/// keep the field honest (see [`StorePlan::missing_count`]). Pass `0` when
/// driving the pure core directly with fully priceable candidates.
pub fn optimise_core(
    input: &OptimiserInput,
    candidates: &[Candidate],
    no_offer_count: usize,
) -> CorePlans {
    optimise_core_with_candidates(input, candidates, no_offer_count).plans
}

fn optimise_core_with_candidates(
    input: &OptimiserInput,
    candidates: &[Candidate],
    no_offer_count: usize,
) -> CoreComputation {
    // Group candidates by item, keeping the cheapest per (item, retailer).
    // Map: item_index -> retailer -> best candidate.
    let mut best_per_item_retailer: BTreeMap<usize, HashMap<Retailer, Candidate>> = BTreeMap::new();
    for cand in candidates {
        let by_retailer = best_per_item_retailer.entry(cand.item_index).or_default();
        match by_retailer.get(&cand.retailer) {
            Some(existing) if !is_cheaper(cand, existing) => {}
            _ => {
                by_retailer.insert(cand.retailer, cand.clone());
            }
        }
    }

    // The set of items that have at least one candidate somewhere. Items the
    // caller flagged as unresolved/no-offer never appear here.
    let priced_items: Vec<usize> = best_per_item_retailer.keys().copied().collect();

    // The retailers that appear anywhere and are eligible for this run, in a
    // stable order. Stores the household set to `off` (or left `occasional`
    // without a trip today) never seed a plan, so they cannot appear anywhere in
    // the output — including the Dunnes spend-and-save voucher hint, which is
    // driven purely by the Dunnes lines a plan carries (issue #98).
    let mut all_retailers: Vec<Retailer> = best_per_item_retailer
        .values()
        .flat_map(|m| m.keys().copied())
        .filter(|&r| input.is_eligible(r))
        .collect();
    all_retailers.sort_by_key(|r| r.as_db_str());
    all_retailers.dedup();

    // ---- Single-store plans: one per retailer, ranked by the fulfilment rule.
    let single_plans: Vec<StorePlan> = all_retailers
        .iter()
        .map(|&r| {
            build_single_store_plan(
                input,
                &best_per_item_retailer,
                &priced_items,
                r,
                no_offer_count,
            )
        })
        .collect();
    let best_single = pick_best_plan(single_plans.clone());

    // ---- Two-store plans: every unordered pair, each item taken at whichever of
    // the two stores is cheaper. Ranked by the same fulfilment rule, but only
    // genuine two-retailer splits are eligible for `best_two_store` (see
    // `pick_best_two_store_plan`): a pair that degenerates to one store is not a
    // two-store plan.
    let mut two_store_plans: Vec<StorePlan> = Vec::new();
    for i in 0..all_retailers.len() {
        for j in (i + 1)..all_retailers.len() {
            two_store_plans.push(build_two_store_plan(
                input,
                &best_per_item_retailer,
                &priced_items,
                all_retailers[i],
                all_retailers[j],
                no_offer_count,
            ));
        }
    }
    let best_two_store = pick_best_two_store_plan(&two_store_plans);

    // ---- Theoretical minimum: each item at its globally cheapest retailer.
    let theoretical_minimum = build_theoretical_minimum(
        input,
        &best_per_item_retailer,
        &priced_items,
        no_offer_count,
    );

    let genuine_two_store_candidates: Vec<StorePlan> = two_store_plans
        .into_iter()
        .filter(|plan| plan.stores.len() == 2)
        .collect();
    CoreComputation {
        plans: CorePlans {
            best_single,
            best_two_store,
            theoretical_minimum,
        },
        two_store_candidates: genuine_two_store_candidates,
        single_plans,
        best_per_item: best_per_item_retailer,
        priced_items,
    }
}

/// Whether candidate `a` is strictly preferable to `b` for the same item, using
/// price as the primary key and `accepted` status as the only tie-breaker.
fn is_cheaper(a: &Candidate, b: &Candidate) -> bool {
    let (pa, pb) = (a.effective_unit_price(), b.effective_unit_price());
    if pa != pb {
        return pa < pb;
    }
    // Equal price: prefer an accepted match over a merely suggested one.
    a.accepted && !b.accepted
}

/// Build a [`PlanLine`] for one candidate under the given basket item.
fn plan_line(input: &OptimiserInput, cand: &Candidate) -> PlanLine {
    let item = &input.items[cand.item_index];
    let unit_price = cand.effective_unit_price();
    let line_total = unit_price * i64::from(item.count);
    PlanLine {
        item_index: cand.item_index,
        entry_id: item.entry_id.clone(),
        display_text: item.display_text.clone(),
        product_name: cand.product.name.clone(),
        canonical_item_id: item.canonical_item_id.clone(),
        retailer: cand.retailer,
        retailer_product_id: cand.product.id.clone(),
        offer_id: cand.offer.id.clone(),
        unit_price_cents: unit_price,
        line_total_cents: line_total,
        loyalty_applied: cand.loyalty_applied(),
        freshness_confidence: freshness_confidence(
            input.current_source_run_number,
            cand.offer.source_successful_run_number_at_fetch,
        ),
    }
}

/// Assemble a [`StorePlan`] from chosen lines and the set of stores it spans.
/// Computes `total_cents`, `missing_count`, and the Dunnes voucher flag.
///
/// `missing_count = (priceable items this plan did not fulfil) + `extra_missing``,
/// where `extra_missing` is the count of resolved-but-unpriceable items
/// ([`OptimiserOutput::no_valid_offer_items`]) — missing at every store, so a
/// constant added to every plan (see [`StorePlan::missing_count`]).
fn assemble_plan(
    lines: Vec<PlanLine>,
    stores: Vec<Retailer>,
    total_items: usize,
    extra_missing: usize,
) -> StorePlan {
    let total_cents: Cents = lines.iter().map(|l| l.line_total_cents).sum();
    let missing_count = (total_items - lines.len()) + extra_missing;
    let dunnes_voucher_offer_text = dunnes_voucher_text(&lines);
    StorePlan {
        stores,
        lines,
        total_cents,
        missing_count,
        dunnes_voucher_offer_text,
    }
}

/// Build the single-store plan for `retailer`: take every item that `retailer`
/// can fulfil, skip the rest (they count as missing).
fn build_single_store_plan(
    input: &OptimiserInput,
    best: &BTreeMap<usize, HashMap<Retailer, Candidate>>,
    priced_items: &[usize],
    retailer: Retailer,
    no_offer_count: usize,
) -> StorePlan {
    let mut lines = Vec::new();
    for &item_index in priced_items {
        if !input.retailer_can_price_item(retailer, item_index) {
            continue;
        }
        if let Some(cand) = best.get(&item_index).and_then(|m| m.get(&retailer)) {
            lines.push(plan_line(input, cand));
        }
    }
    assemble_plan(lines, vec![retailer], priced_items.len(), no_offer_count)
}

/// Build the two-store plan for `(a, b)`: each item is taken at whichever of the
/// two stores offers the lower effective price; items neither store stocks are
/// missing. The reported `stores` are exactly the retailers actually used, so a
/// pair where one store wins every item degenerates to a one-store `stores`
/// vector. Such degenerate plans are filtered out by [`pick_best_two_store_plan`]
/// before `best_two_store` is chosen, guaranteeing it always spans two stores.
fn build_two_store_plan(
    input: &OptimiserInput,
    best: &BTreeMap<usize, HashMap<Retailer, Candidate>>,
    priced_items: &[usize],
    a: Retailer,
    b: Retailer,
    no_offer_count: usize,
) -> StorePlan {
    let mut lines = Vec::new();
    let mut used_a = false;
    let mut used_b = false;
    for &item_index in priced_items {
        let by_retailer = match best.get(&item_index) {
            Some(m) => m,
            None => continue,
        };
        let ca = by_retailer
            .get(&a)
            .filter(|_| input.retailer_can_price_item(a, item_index));
        let cb = by_retailer
            .get(&b)
            .filter(|_| input.retailer_can_price_item(b, item_index));
        let chosen = match (ca, cb) {
            (Some(ca), Some(cb)) => {
                if is_cheaper(cb, ca) {
                    cb
                } else {
                    ca
                }
            }
            (Some(ca), None) => ca,
            (None, Some(cb)) => cb,
            (None, None) => continue,
        };
        if chosen.retailer == a {
            used_a = true;
        } else {
            used_b = true;
        }
        lines.push(plan_line(input, chosen));
    }

    let mut stores = Vec::new();
    if used_a {
        stores.push(a);
    }
    if used_b {
        stores.push(b);
    }
    assemble_plan(lines, stores, priced_items.len(), no_offer_count)
}

/// Build the theoretical minimum: each item at its globally cheapest retailer,
/// regardless of how many distinct stores that implies.
fn build_theoretical_minimum(
    input: &OptimiserInput,
    best: &BTreeMap<usize, HashMap<Retailer, Candidate>>,
    priced_items: &[usize],
    no_offer_count: usize,
) -> Option<StorePlan> {
    let mut lines = Vec::new();
    let mut stores: Vec<Retailer> = Vec::new();
    for &item_index in priced_items {
        let by_retailer = match best.get(&item_index) {
            Some(m) => m,
            None => continue,
        };
        // The globally cheapest candidate for this item among eligible stores
        // that its category rules allow. An ineligible store (off, or occasional
        // without a trip today) or one the household's category rules exclude is
        // never routed, so it cannot appear in the benchmark either (issues #98,
        // #99).
        //
        // The final retailer tie-break keeps the choice independent of HashMap
        // iteration order: the benchmark names one store per item, and the reasons
        // derived from it must be reproducible run to run (issue #103).
        let cheapest = by_retailer
            .values()
            .filter(|c| input.retailer_can_price_item(c.retailer, item_index))
            .min_by(|a, b| {
                a.effective_unit_price()
                    .cmp(&b.effective_unit_price())
                    .then_with(|| b.accepted.cmp(&a.accepted))
                    .then_with(|| a.retailer.as_db_str().cmp(b.retailer.as_db_str()))
            });
        let Some(cheapest) = cheapest else {
            // No eligible store prices this item; it is missing from the
            // benchmark (and counted missing via `priced_items.len()`).
            continue;
        };
        if !stores.contains(&cheapest.retailer) {
            stores.push(cheapest.retailer);
        }
        lines.push(plan_line(input, cheapest));
    }
    if lines.is_empty() {
        return None;
    }
    stores.sort_by_key(|r| r.as_db_str());
    Some(assemble_plan(
        lines,
        stores,
        priced_items.len(),
        no_offer_count,
    ))
}

/// Pick the best plan by the fulfilment rule `(missing_count ASC, total_cost
/// ASC)`. Returns `None` if the candidate list is empty or every plan priced
/// nothing (an all-missing plan is never a usable result).
fn pick_best_plan(plans: Vec<StorePlan>) -> Option<StorePlan> {
    plans
        .into_iter()
        .filter(|p| !p.lines.is_empty())
        .min_by(|a, b| {
            a.missing_count
                .cmp(&b.missing_count)
                .then(a.total_cents.cmp(&b.total_cents))
        })
}

/// Pick the best *genuine* two-store plan: like [`pick_best_plan`], but only
/// plans that span exactly two distinct stores are eligible.
///
/// A candidate pair where one retailer wins (or solely stocks) every priced item
/// reports `stores.len() == 1`; such a plan is a single-store plan in disguise
/// and must not be returned as `best_two_store` (the acceptance criterion requires
/// `best_two_store.stores` to hold exactly two retailers). When no pair yields a
/// genuine split — e.g. one store dominates the basket, or there is a single
/// priceable item — this returns `None` and `best_two_store` is absent.
fn pick_best_two_store_plan(plans: &[StorePlan]) -> Option<StorePlan> {
    plans
        .iter()
        .filter(|p| p.stores.len() == 2)
        .min_by(|a, b| {
            a.missing_count
                .cmp(&b.missing_count)
                .then(a.total_cents.cmp(&b.total_cents))
        })
        .cloned()
}

/// Compute freshness confidence from the current run number and the run number
/// recorded on the offer at fetch time.
///
/// `missed_cycles = current − at_fetch`; `0 ⇒ High`, `1 ⇒ Low`, `≥2 ⇒ Unknown`.
/// If *either* number is `NULL` we cannot compute a delta, so the result is
/// [`FreshnessConfidence::Unknown`] — we never guess freshness from a missing
/// run number. A negative delta (a fetch stamped ahead of the current number,
/// which should not happen) also collapses to `Unknown` rather than pretending
/// to be fresh.
fn freshness_confidence(current: Option<i64>, at_fetch: Option<i64>) -> FreshnessConfidence {
    match (current, at_fetch) {
        (Some(current), Some(at_fetch)) => match current - at_fetch {
            0 => FreshnessConfidence::High,
            1 => FreshnessConfidence::Low,
            _ => FreshnessConfidence::Unknown,
        },
        _ => FreshnessConfidence::Unknown,
    }
}

/// Build the Dunnes spend-and-save prompt for a plan's lines, if applicable.
///
/// Sums the Dunnes lines; if the Dunnes spend falls in
/// `[DUNNES_VOUCHER_LOW, DUNNES_VOUCHER_HIGH]` (€45–€50 inclusive) returns the
/// prompt "add €X.XX more to potentially trigger the €50 voucher", where
/// `X = €50 − dunnes_spend`. At exactly €50 the "add €0.00" prompt still fires
/// (the upper bound is inclusive): the household is on the threshold.
///
/// This is modelled as a *future-shop* incentive (design §10) — it is never
/// subtracted from the current total, only flagged.
fn dunnes_voucher_text(lines: &[PlanLine]) -> Option<String> {
    let dunnes_spend: Cents = lines
        .iter()
        .filter(|l| l.retailer == Retailer::Dunnes)
        .map(|l| l.line_total_cents)
        .sum();
    if (DUNNES_VOUCHER_LOW..=DUNNES_VOUCHER_HIGH).contains(&dunnes_spend) {
        let shortfall = DUNNES_VOUCHER_HIGH - dunnes_spend;
        Some(format!(
            "add €{}.{:02} more to potentially trigger the €50 voucher",
            shortfall / 100,
            shortfall % 100
        ))
    } else {
        None
    }
}

/// A two-store split that was considered but not recommended, with the facts the
/// reason generator needs to explain the rejection.
#[derive(Debug, Clone, PartialEq)]
struct RejectedSplit {
    /// Stores the split would add beyond the chosen single-store plan.
    stores: Vec<Retailer>,
    /// Items the chosen single-store plan prices that the split would drop.
    dropped_items: Vec<usize>,
    /// `single.total − split.total`: positive means the split is cheaper.
    saving_cents: Cents,
    /// The effective convenience bar the split failed to clear. Only meaningful
    /// when the split prices the same items (otherwise coverage, not cost,
    /// decided and no like-for-like saving may be reported).
    threshold_cents: Cents,
    /// The split prices exactly the same items as the single store, so its total
    /// is directly comparable (a like-for-like saving).
    same_items: bool,
    /// The split reaches fewer items than the single-store plan.
    less_complete: bool,
}

/// The recommendation plus the decision context the reason generator needs.
///
/// Kept separate from the wire-facing [`Recommendation`] so the extra explanation
/// inputs (effective bar, rejected split, gained items) never leak into the
/// persisted plan JSON or the API contract.
#[derive(Debug, Clone, PartialEq)]
struct RecommendationDecision {
    /// The chosen recommendation, or `None` when no plan could be priced.
    recommendation: Option<Recommendation>,
    /// The effective second-store bar applied to the decisive comparison, in
    /// cents (the lower nearby bar when a nearby store was added).
    threshold_cents: Cents,
    /// True when that bar is the lower nearby one (`threshold_cents` below the
    /// household's stored bar).
    nearby_applied: bool,
    /// The best split that was rejected, when the single store was recommended.
    rejected_split: Option<RejectedSplit>,
    /// Items the recommended split reaches that the best single store cannot —
    /// why an extra stop is worth it even when the saving is below the bar.
    gained_items: Vec<usize>,
}

/// Apply the convenience threshold to choose between the best single store and
/// the best two-store split, and record why the alternative lost.
///
/// Completeness comes first: a two-store split is only ever preferred when it is
/// *at least as complete* as the single store (`missing_count` no greater). Only
/// then does cost matter, and only if the saving strictly exceeds
/// `threshold_2_cents`. Otherwise the single store is recommended. This is the
/// fulfilment rule again: cost can never override missing-item count.
///
/// `nearby` lists the retailers the household marked as close by (issue #98).
/// The lower bar applies only to stores added beyond the single-store plan.
fn decide_recommendation(
    plans: &CorePlans,
    two_store_candidates: &[StorePlan],
    threshold_2_cents: Cents,
    nearby: &[Retailer],
) -> RecommendationDecision {
    // The nearby-aware bar for a split relative to the chosen single store, and
    // whether the lower nearby bar is the one in force.
    let bar_for = |single: &StorePlan, two: &StorePlan| -> (Cents, bool) {
        let added_store_is_nearby = two
            .stores
            .iter()
            .filter(|store| !single.stores.contains(store))
            .any(|store| nearby.contains(store));
        if added_store_is_nearby {
            (threshold_2_cents.min(NEARBY_THRESHOLD_2_CENTS), true)
        } else {
            (threshold_2_cents, false)
        }
    };

    match (&plans.best_single, &plans.best_two_store) {
        (Some(single), Some(best_two)) => {
            let best_saving = single.total_cents - best_two.total_cents;
            let qualifying_two = two_store_candidates
                .iter()
                .filter_map(|two| {
                    let saving = single.total_cents - two.total_cents;
                    let more_complete = two.missing_count < single.missing_count;
                    let equal_complete = two.missing_count == single.missing_count;
                    let single_items: BTreeSet<usize> =
                        single.lines.iter().map(|l| l.item_index).collect();
                    let two_items: BTreeSet<usize> =
                        two.lines.iter().map(|l| l.item_index).collect();
                    let same_items = single_items == two_items;
                    let (threshold, nearby_applied) = bar_for(single, two);
                    let clears_threshold = same_items && saving > threshold;
                    (more_complete || (equal_complete && clears_threshold)).then_some((
                        two,
                        saving,
                        equal_complete && clears_threshold,
                        threshold,
                        nearby_applied,
                        same_items,
                    ))
                })
                .min_by(|(a, ..), (b, ..)| {
                    a.missing_count
                        .cmp(&b.missing_count)
                        .then(a.total_cents.cmp(&b.total_cents))
                });

            if let Some((two, saving, clears, threshold, nearby_applied, same_items)) =
                qualifying_two
            {
                let gained_items: Vec<usize> = two
                    .lines
                    .iter()
                    .map(|l| l.item_index)
                    .filter(|i| !single.lines.iter().any(|sl| sl.item_index == *i))
                    .collect();
                RecommendationDecision {
                    recommendation: Some(Recommendation {
                        kind: RecommendedKind::TwoStore,
                        plan: two.clone(),
                        clears_convenience_threshold: clears,
                        two_store_saving_cents: if same_items { saving } else { 0 },
                    }),
                    threshold_cents: threshold,
                    nearby_applied,
                    rejected_split: None,
                    gained_items: if clears { Vec::new() } else { gained_items },
                }
            } else {
                let (threshold, nearby_applied) = bar_for(single, best_two);
                let single_items: BTreeSet<usize> =
                    single.lines.iter().map(|l| l.item_index).collect();
                let two_items: BTreeSet<usize> =
                    best_two.lines.iter().map(|l| l.item_index).collect();
                let dropped_items: Vec<usize> =
                    single_items.difference(&two_items).copied().collect();
                let added_stores: Vec<Retailer> = best_two
                    .stores
                    .iter()
                    .copied()
                    .filter(|store| !single.stores.contains(store))
                    .collect();
                RecommendationDecision {
                    recommendation: Some(Recommendation {
                        kind: RecommendedKind::SingleStore,
                        plan: single.clone(),
                        clears_convenience_threshold: false,
                        two_store_saving_cents: if single_items == two_items {
                            best_saving
                        } else {
                            0
                        },
                    }),
                    threshold_cents: threshold,
                    nearby_applied,
                    rejected_split: Some(RejectedSplit {
                        stores: added_stores,
                        dropped_items,
                        saving_cents: if single_items == two_items {
                            best_saving
                        } else {
                            0
                        },
                        threshold_cents: threshold,
                        same_items: single_items == two_items,
                        less_complete: best_two.missing_count > single.missing_count,
                    }),
                    gained_items: Vec::new(),
                }
            }
        }
        (Some(single), None) => RecommendationDecision {
            recommendation: Some(Recommendation {
                kind: RecommendedKind::SingleStore,
                // No two-store option: saving is undefined; report 0.
                two_store_saving_cents: 0,
                plan: single.clone(),
                clears_convenience_threshold: false,
            }),
            threshold_cents: threshold_2_cents,
            nearby_applied: false,
            rejected_split: None,
            gained_items: Vec::new(),
        },
        (None, Some(two)) => RecommendationDecision {
            recommendation: Some(Recommendation {
                kind: RecommendedKind::TwoStore,
                two_store_saving_cents: 0,
                plan: two.clone(),
                clears_convenience_threshold: false,
            }),
            threshold_cents: threshold_2_cents,
            nearby_applied: false,
            rejected_split: None,
            gained_items: Vec::new(),
        },
        (None, None) => RecommendationDecision {
            recommendation: None,
            threshold_cents: threshold_2_cents,
            nearby_applied: false,
            rejected_split: None,
            gained_items: Vec::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Plan reasons (issue #103)
// ---------------------------------------------------------------------------

/// Construct one [`PlanReason`] (keeps the call sites readable).
#[allow(clippy::too_many_arguments)]
fn reason(
    plan: PlanKind,
    code: ReasonCode,
    item_indices: Vec<usize>,
    retailers: Vec<Retailer>,
    cents: Option<Cents>,
    threshold_cents: Option<Cents>,
    snapshot_date: Option<String>,
) -> PlanReason {
    PlanReason {
        plan,
        code,
        item_indices,
        retailers,
        cents,
        threshold_cents,
        snapshot_date,
    }
}

/// The eligible stores that may price `item` (store eligibility plus its
/// per-store category rules, issue #99), in `as_db_str` order. Empty when the
/// item has no candidate at all or no store is allowed to take it.
fn eligible_stores_for(
    input: &OptimiserInput,
    best_per_item: &BTreeMap<usize, HashMap<Retailer, Candidate>>,
    item: usize,
) -> Vec<Retailer> {
    let mut stores: Vec<Retailer> = best_per_item
        .get(&item)
        .map(|by_retailer| {
            by_retailer
                .keys()
                .copied()
                .filter(|r| input.retailer_can_price_item(*r, item))
                .collect()
        })
        .unwrap_or_default();
    stores.sort_by_key(|r| r.as_db_str());
    stores
}

/// Whether `item` carries a brand constraint (policy or brand lists), so a plan
/// line for it is explained by the household's brand rule.
fn has_brand_rule(item: &BasketItem) -> bool {
    matches!(
        item.constraint_policy,
        ConstraintPolicy::Brand | ConstraintPolicy::AllowedSet | ConstraintPolicy::OwnLabel
    ) || item.constraints.as_ref().is_some_and(|c| {
        c.brand.is_some()
            || c.allowed_brands.as_ref().is_some_and(|b| !b.is_empty())
            || c.excluded_brands.as_ref().is_some_and(|b| !b.is_empty())
    })
}

/// Stores in `stores` that are not part of the single-store baseline — the stops
/// a split would add.
fn added_stores(
    stores: impl Iterator<Item = Retailer>,
    single: Option<&StorePlan>,
) -> Vec<Retailer> {
    let single_stores: &[Retailer] = single.map(|p| p.stores.as_slice()).unwrap_or(&[]);
    stores.filter(|s| !single_stores.contains(s)).collect()
}

/// Shared inputs for per-plan reason generation.
struct ReasonContext<'a> {
    input: &'a OptimiserInput,
    comp: &'a CoreComputation,
    decision: &'a RecommendationDecision,
    classification: &'a Classification,
    /// Item → globally cheapest eligible store (from the theoretical minimum).
    theo_store: BTreeMap<usize, Retailer>,
}

/// Build the deterministic reasons for all published plans (issue #103).
///
/// Each plan's reasons are ordered most-important-first.
/// Reasons are derived from the plan's actual lines plus the run's
/// constraints, threshold and known price dates, so they are traceable and
/// reproducible — never generated text.
fn build_reasons(
    input: &OptimiserInput,
    comp: &CoreComputation,
    decision: &RecommendationDecision,
    classification: &Classification,
) -> Vec<PlanReason> {
    let theo_store: BTreeMap<usize, Retailer> = comp
        .plans
        .theoretical_minimum
        .as_ref()
        .map(|p| p.lines.iter().map(|l| (l.item_index, l.retailer)).collect())
        .unwrap_or_default();
    let ctx = ReasonContext {
        input,
        comp,
        decision,
        classification,
        theo_store,
    };

    let mut reasons: Vec<PlanReason> = Vec::new();
    if let Some(rec) = decision.recommendation.as_ref() {
        reasons.extend(reasons_for_plan(PlanKind::Recommended, &rec.plan, &ctx));
    }
    if let Some(plan) = comp.plans.best_single.as_ref() {
        reasons.extend(reasons_for_plan(PlanKind::BestSingle, plan, &ctx));
    }
    if let Some(plan) = comp.plans.best_two_store.as_ref() {
        reasons.extend(reasons_for_plan(PlanKind::BestTwoStore, plan, &ctx));
    }
    if let Some(plan) = comp.plans.theoretical_minimum.as_ref() {
        reasons.extend(reasons_for_plan(PlanKind::TheoreticalMinimum, plan, &ctx));
    }
    reasons
}

/// The ordered reasons for one plan. See [`build_reasons`] for the contract.
fn reasons_for_plan(kind: PlanKind, plan: &StorePlan, ctx: &ReasonContext) -> Vec<PlanReason> {
    let mut out: Vec<PlanReason> = Vec::new();
    let single = ctx.comp.plans.best_single.as_ref();
    let line_items: BTreeSet<usize> = plan.lines.iter().map(|l| l.item_index).collect();

    // ---- Why this plan was chosen (the recommendation card only). ----------
    if kind == PlanKind::Recommended {
        if let Some(rec) = ctx.decision.recommendation.as_ref() {
            match rec.kind {
                RecommendedKind::TwoStore => {
                    let added = added_stores(rec.plan.stores.iter().copied(), single);
                    if rec.clears_convenience_threshold {
                        out.push(reason(
                            kind,
                            ReasonCode::ThresholdCleared,
                            Vec::new(),
                            added,
                            Some(rec.two_store_saving_cents),
                            Some(ctx.decision.threshold_cents),
                            None,
                        ));
                    } else if !ctx.decision.gained_items.is_empty() {
                        // Chosen below the bar because it reaches items the single
                        // store cannot — the extra stop buys completeness, not cost.
                        out.push(reason(
                            kind,
                            ReasonCode::MoreComplete,
                            ctx.decision.gained_items.clone(),
                            added,
                            None,
                            None,
                            None,
                        ));
                    }
                }
                RecommendedKind::SingleStore => {
                    if let Some(rejected) = ctx.decision.rejected_split.as_ref() {
                        if rejected.less_complete && !rejected.dropped_items.is_empty() {
                            out.push(reason(
                                kind,
                                ReasonCode::SplitDropsItems,
                                rejected.dropped_items.clone(),
                                rejected.stores.clone(),
                                None,
                                None,
                                None,
                            ));
                        } else if rejected.same_items {
                            // Like-for-like: the same items, so the saving is honest.
                            out.push(reason(
                                kind,
                                ReasonCode::ThresholdNotCleared,
                                Vec::new(),
                                rejected.stores.clone(),
                                Some(rejected.saving_cents),
                                Some(rejected.threshold_cents),
                                None,
                            ));
                        } else {
                            // Equal counts are not the same basket: no saving claim.
                            out.push(reason(
                                kind,
                                ReasonCode::CoverageDiffers,
                                Vec::new(),
                                rejected.stores.clone(),
                                None,
                                None,
                                None,
                            ));
                        }
                    }
                }
            }
        }
    }

    // ---- Which stores are cheapest for which of the plan's lines. ----------
    for &retailer in &plan.stores {
        let cheapest: Vec<usize> = plan
            .lines
            .iter()
            .filter(|l| l.retailer == retailer)
            .map(|l| l.item_index)
            .filter(|i| {
                // Only when a rival store could have stocked it: otherwise the
                // stronger "only store we have a price for" reason covers it.
                ctx.theo_store.get(i) == Some(&retailer)
                    && eligible_stores_for(ctx.input, &ctx.comp.best_per_item, *i).len() > 1
            })
            .collect();
        if !cheapest.is_empty() {
            out.push(reason(
                kind,
                ReasonCode::CheapestAtStore,
                cheapest,
                vec![retailer],
                None,
                None,
                None,
            ));
        }
    }

    // ---- Lines only one eligible store prices at all. ----------------------
    for &retailer in &plan.stores {
        let only: Vec<usize> = plan
            .lines
            .iter()
            .filter(|l| l.retailer == retailer)
            .map(|l| l.item_index)
            .filter(|i| {
                let stores = eligible_stores_for(ctx.input, &ctx.comp.best_per_item, *i);
                stores.len() == 1 && stores[0] == retailer
            })
            .collect();
        if !only.is_empty() {
            out.push(reason(
                kind,
                ReasonCode::OnlyStoreStocking,
                only,
                vec![retailer],
                None,
                None,
                None,
            ));
        }
    }

    // ---- Coverage: how much of the basket this plan actually prices. -------
    out.push(reason(
        kind,
        ReasonCode::Coverage,
        plan.lines.iter().map(|l| l.item_index).collect(),
        plan.stores.clone(),
        Some(plan.total_cents),
        None,
        None,
    ));

    // ---- Items this plan cannot price, distinguishing the two cases. -------
    let missing_here: Vec<usize> = ctx
        .comp
        .priced_items
        .iter()
        .copied()
        .filter(|i| !line_items.contains(i))
        .collect();
    if !missing_here.is_empty() {
        out.push(reason(
            kind,
            ReasonCode::NoKnownPriceHere,
            missing_here,
            plan.stores.clone(),
            None,
            None,
            None,
        ));
    }
    let no_offer: Vec<usize> = ctx
        .classification
        .no_valid_offer
        .iter()
        .map(|i| i.item_index)
        .collect();
    if !no_offer.is_empty() {
        out.push(reason(
            kind,
            ReasonCode::NoKnownPriceAnywhere,
            no_offer,
            Vec::new(),
            None,
            None,
            None,
        ));
    }
    let unresolved: Vec<usize> = ctx
        .classification
        .unresolved
        .iter()
        .map(|i| i.item_index)
        .collect();
    if !unresolved.is_empty() {
        out.push(reason(
            kind,
            ReasonCode::UnresolvedItems,
            unresolved,
            Vec::new(),
            None,
            None,
            None,
        ));
    }

    // ---- Lines governed by the household's brand rule. ---------------------
    let brand_items: Vec<usize> = plan
        .lines
        .iter()
        .map(|l| l.item_index)
        .filter(|i| ctx.input.items.get(*i).is_some_and(has_brand_rule))
        .collect();
    if !brand_items.is_empty() {
        out.push(reason(
            kind,
            ReasonCode::BrandRule,
            brand_items,
            Vec::new(),
            None,
            None,
            None,
        ));
    }

    // ---- Whole-store alternatives considered and rejected. -----------------
    if matches!(kind, PlanKind::Recommended | PlanKind::BestSingle) {
        for single_plan in &ctx.comp.single_plans {
            let Some(&retailer) = single_plan.stores.first() else {
                continue;
            };
            if plan.stores.contains(&retailer) {
                continue;
            }
            let single_items: BTreeSet<usize> =
                single_plan.lines.iter().map(|l| l.item_index).collect();
            let plan_items: BTreeSet<usize> = plan.lines.iter().map(|l| l.item_index).collect();
            if single_items == plan_items {
                // Like-for-like: report the difference (negative = it would cost
                // more than this plan).
                out.push(reason(
                    kind,
                    ReasonCode::RejectedStore,
                    Vec::new(),
                    vec![retailer],
                    Some(plan.total_cents - single_plan.total_cents),
                    None,
                    None,
                ));
            } else {
                out.push(reason(
                    kind,
                    ReasonCode::CoverageDiffers,
                    Vec::new(),
                    vec![retailer],
                    None,
                    None,
                    None,
                ));
            }
        }
    }

    // ---- Price freshness: the date the plan's prices are from, plus any line
    // behind the latest successful source run. ------------------------------
    let snapshots: Vec<(usize, Option<String>)> = plan
        .lines
        .iter()
        .map(|line| {
            let snapshot = ctx
                .comp
                .best_per_item
                .get(&line.item_index)
                .and_then(|by_retailer| by_retailer.get(&line.retailer))
                .and_then(|candidate| candidate.offer.source_snapshot_date.clone());
            (line.item_index, snapshot)
        })
        .collect();
    let as_of = snapshots
        .iter()
        .filter_map(|(_, snapshot)| snapshot.as_deref())
        .min()
        .map(str::to_owned);
    let undated_items = snapshots
        .iter()
        .filter_map(|(item_index, snapshot)| snapshot.is_none().then_some(*item_index))
        .collect();
    out.push(reason(
        kind,
        ReasonCode::PriceAsOf,
        undated_items,
        Vec::new(),
        None,
        None,
        as_of,
    ));

    let stale_lines: Vec<&PlanLine> = plan
        .lines
        .iter()
        .filter(|line| {
            let Some(offer) = ctx
                .comp
                .best_per_item
                .get(&line.item_index)
                .and_then(|by_retailer| by_retailer.get(&line.retailer))
                .map(|candidate| &candidate.offer)
            else {
                return false;
            };
            match (
                ctx.input.current_source_run_number,
                offer.source_successful_run_number_at_fetch,
            ) {
                (Some(current), Some(fetched)) => current > fetched,
                _ => false,
            }
        })
        .collect();
    if !stale_lines.is_empty() {
        let stale_retailers: Vec<Retailer> = stale_lines.iter().map(|line| line.retailer).fold(
            Vec::new(),
            |mut retailers, retailer| {
                if !retailers.contains(&retailer) {
                    retailers.push(retailer);
                }
                retailers
            },
        );
        out.push(reason(
            kind,
            ReasonCode::StalePrices,
            stale_lines.iter().map(|line| line.item_index).collect(),
            stale_retailers,
            None,
            None,
            None,
        ));
    }

    // ---- Stores left out by the household's eligibility settings. ----------
    if kind == PlanKind::Recommended {
        for retailer in Retailer::ALL {
            match ctx.input.store_status(retailer) {
                StoreStatus::Off => out.push(reason(
                    kind,
                    ReasonCode::StoreOff,
                    Vec::new(),
                    vec![retailer],
                    None,
                    None,
                    None,
                )),
                StoreStatus::Occasional if !ctx.input.trip_retailers.contains(&retailer) => out
                    .push(reason(
                        kind,
                        ReasonCode::StoreNotOnTrip,
                        Vec::new(),
                        vec![retailer],
                        None,
                        None,
                        None,
                    )),
                _ => {
                    // The store takes part in this run, but its category rules
                    // exclude some items it stocks (issue #99): name them so the
                    // household sees why a cheaper store was skipped.
                    let excluded: Vec<usize> = ctx
                        .classification
                        .category_excluded
                        .iter()
                        .filter_map(|&(item, excluded_retailer)| {
                            (excluded_retailer == retailer).then_some(item)
                        })
                        .collect();
                    if !excluded.is_empty() {
                        out.push(reason(
                            kind,
                            ReasonCode::CategoryExcluded,
                            excluded,
                            vec![retailer],
                            None,
                            None,
                            None,
                        ));
                    }
                }
            }
        }
    }

    out.sort_by_key(|reason| match reason.code {
        ReasonCode::ThresholdCleared
        | ReasonCode::ThresholdNotCleared
        | ReasonCode::SplitDropsItems
        | ReasonCode::MoreComplete => 0,
        ReasonCode::CoverageDiffers | ReasonCode::RejectedStore => 1,
        ReasonCode::Coverage => 2,
        ReasonCode::NoKnownPriceHere
        | ReasonCode::NoKnownPriceAnywhere
        | ReasonCode::UnresolvedItems => 3,
        ReasonCode::PriceAsOf | ReasonCode::StalePrices => 4,
        ReasonCode::BrandRule => 4,
        ReasonCode::StoreOff | ReasonCode::StoreNotOnTrip | ReasonCode::CategoryExcluded => 5,
        ReasonCode::CheapestAtStore | ReasonCode::OnlyStoreStocking => 6,
    });
    out
}

// ---------------------------------------------------------------------------
// Item classification (resolved / unresolved / no-offer)
// ---------------------------------------------------------------------------

/// Per-item classification result, separating items that can be priced from
/// those surfaced as unresolved, no-offer, or awaiting confirmation.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    /// Items with no `canonical_item_id`.
    pub unresolved: Vec<UnresolvedItem>,
    /// Resolved items that produced no valid offer.
    pub no_valid_offer: Vec<NoValidOfferItem>,
    /// Resolved, priceable items held back under `substitution_policy = 'ask'`.
    pub needs_confirmation: Vec<NeedsConfirmationItem>,
    /// Item/store pairs removed by per-store category rules.
    pub category_excluded: Vec<(usize, Retailer)>,
}

/// Human-readable pack/size text for a candidate product. Prefers the retailer's
/// own `pack_size_text` (such as `500ml` or `4 x 410g`) and falls back to a
/// composed `size_value` + `size_unit` (so `500.0` + `ml` becomes `500ml`).
/// Returns `None` when the product carries neither, so the UI omits the size
/// rather than rendering an empty token.
fn size_text(product: &RetailerProduct) -> Option<String> {
    if let Some(text) = product.pack_size_text.as_deref() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    match (product.size_value, product.size_unit.as_deref()) {
        (Some(value), Some(unit)) => {
            // Drop a trailing ".0" so 500.0 reads "500ml", not "500.0ml".
            let value = if value.fract() == 0.0 {
                format!("{}", value as i64)
            } else {
                format!("{value}")
            };
            Some(format!("{value}{unit}"))
        }
        _ => None,
    }
}

/// Project a list of priced [`Candidate`]s into the wire-facing [`CandidateOffer`]
/// the household chooses between (issue #78). Sorted cheapest-effective-price
/// first, then by retailer then product id, so the chooser ordering is
/// deterministic (mirrors the optimiser's own per-retailer cheapest-first pick).
fn candidate_offers(candidates: &[Candidate]) -> Vec<CandidateOffer> {
    let mut offers: Vec<CandidateOffer> = candidates
        .iter()
        .map(|c| CandidateOffer {
            retailer_product_id: c.product.id.clone(),
            name: c.product.name.clone(),
            brand: c.product.brand.clone(),
            size_text: size_text(&c.product),
            price_cents: c.effective_unit_price(),
            retailer: c.retailer,
        })
        .collect();
    offers.sort_by(|a, b| {
        a.price_cents
            .cmp(&b.price_cents)
            .then_with(|| a.retailer.as_db_str().cmp(b.retailer.as_db_str()))
            .then_with(|| a.retailer_product_id.cmp(&b.retailer_product_id))
    });
    offers
}

/// Classify each basket item against its raw matches/offers, applying the
/// constraint filter, and emit the constraint-passing [`Candidate`]s plus the
/// unresolved / no-offer classification.
///
/// `priced_inputs` supplies, per item index, the `(match, product, offer)`
/// triples already fetched from the DB (or hand-built in tests). The brand-config
/// error short-circuits an item to `no_valid_offer` regardless of its candidates.
///
/// After the per-product field filter, the item's `substitution_policy`
/// (Backlog-T1) decides how the surviving candidates are routed:
/// * `Any` — all candidates are priceable (current behaviour).
/// * `Strict` — only the exact (`accepted`) match's offer may be used; if that
///   exact product has no qualifying offer the item is shown as missing
///   ([`NoOfferReason::StrictNoExactOffer`]) rather than substituted.
/// * `Ask` — any candidate exists, but the item is held back for household
///   confirmation ([`Classification::needs_confirmation`]) instead of being
///   auto-priced, **unless** the household has already confirmed a product for
///   it: `preferred_products[item_index]` names a chosen `retailer_product_id`
///   (issue #78). When that chosen product is among the surviving candidates the
///   item is priced (restricted to that single candidate) and enters the plans;
///   when it is not (the choice no longer qualifies, e.g. its offer lapsed) the
///   item falls back to the confirmation prompt.
fn classify_items(
    input: &OptimiserInput,
    priced_inputs: &BTreeMap<usize, Vec<(ProductMatch, RetailerProduct, ProductOffer)>>,
    preferred_products: &BTreeMap<usize, String>,
) -> (Vec<Candidate>, Classification) {
    let mut candidates = Vec::new();
    let mut unresolved = Vec::new();
    let mut no_valid_offer = Vec::new();
    let mut needs_confirmation = Vec::new();
    let mut category_excluded = Vec::new();

    for (item_index, item) in input.items.iter().enumerate() {
        // 1a. Unresolved: no canonical item id.
        if item.canonical_item_id.is_none() {
            unresolved.push(UnresolvedItem {
                item_index,
                entry_id: item.entry_id.clone(),
                display_text: item.display_text.clone(),
            });
            continue;
        }

        // Resolved from here on: every exit below is a `no_valid_offer` entry,
        // each carrying the canonical id so the UI can link the warning to the
        // item's preferences screen.
        let canonical_item_id = item
            .canonical_item_id
            .clone()
            .expect("resolved item has a canonical id");

        // 1b. No matches / offers at all.
        let Some(rows) = priced_inputs.get(&item_index).filter(|r| !r.is_empty()) else {
            no_valid_offer.push(NoValidOfferItem {
                item_index,
                entry_id: item.entry_id.clone(),
                display_text: item.display_text.clone(),
                canonical_item_id: canonical_item_id.clone(),
                reason: NoOfferReason::NoMatches,
            });
            continue;
        };

        // 1c. Constraint filter. A config error rejects the whole item.
        let mut item_candidates = Vec::new();
        let mut config_error = false;
        let mut has_category_allowed_offer = false;
        for (m, product, offer) in rows {
            if !input.store_allows_item(product.retailer, item_index) {
                let excluded = (item_index, product.retailer);
                if !category_excluded.contains(&excluded) {
                    category_excluded.push(excluded);
                }
                continue;
            }
            has_category_allowed_offer = true;
            match check_constraints(item.constraint_policy, item.constraints.as_ref(), product) {
                ConstraintCheck::ConfigError => {
                    config_error = true;
                    break;
                }
                ConstraintCheck::Reject => {}
                ConstraintCheck::Pass => {
                    item_candidates.push(Candidate {
                        item_index,
                        retailer: product.retailer,
                        product: product.clone(),
                        offer: offer.clone(),
                        accepted: m.status == MatchStatus::Accepted,
                    });
                }
            }
        }

        if config_error {
            no_valid_offer.push(NoValidOfferItem {
                item_index,
                entry_id: item.entry_id.clone(),
                display_text: item.display_text.clone(),
                canonical_item_id: canonical_item_id.clone(),
                reason: NoOfferReason::BrandPolicyMissingBrand,
            });
            continue;
        }

        if item_candidates.is_empty() {
            no_valid_offer.push(NoValidOfferItem {
                item_index,
                entry_id: item.entry_id.clone(),
                display_text: item.display_text.clone(),
                canonical_item_id: canonical_item_id.clone(),
                reason: if has_category_allowed_offer {
                    NoOfferReason::ConstraintsEliminatedAll
                } else {
                    NoOfferReason::StoreCategoryRulesExcludedAll
                },
            });
            continue;
        }

        // Substitution policy (Backlog-T1). Default to the permissive `Any` when
        // the item has no constraints row.
        let substitution_policy = item
            .constraints
            .as_ref()
            .map(|c| c.substitution_policy)
            .unwrap_or(SubstitutionPolicy::Any);

        match substitution_policy {
            SubstitutionPolicy::Any => {
                candidates.append(&mut item_candidates);
            }
            SubstitutionPolicy::Strict => {
                // "Exact match" == any candidate from an `Accepted` match. Only
                // those may be priced; a merely `Suggested` substitute is never
                // offered, so an item whose matches are all suggested is reported
                // StrictNoExactOffer (see `NoOfferReason::StrictNoExactOffer`).
                // Drop non-accepted candidates and, if none remain with a
                // qualifying offer, the item is missing by policy not substituted.
                item_candidates.retain(|c| c.accepted);
                if item_candidates.is_empty() {
                    no_valid_offer.push(NoValidOfferItem {
                        item_index,
                        entry_id: item.entry_id.clone(),
                        display_text: item.display_text.clone(),
                        canonical_item_id: canonical_item_id.clone(),
                        reason: NoOfferReason::StrictNoExactOffer,
                    });
                } else {
                    candidates.append(&mut item_candidates);
                }
            }
            SubstitutionPolicy::Ask => {
                // Issue #78: if the household has already confirmed a product for
                // this item (a `prefer` recorded in match_feedback, surfaced here
                // via `preferred_products`), and that product is still among the
                // qualifying candidates, restrict to it and price the item into
                // the plans. Otherwise hold the item back for confirmation,
                // carrying the candidate offers so the UI can render a chooser.
                let chosen = preferred_products.get(&item_index).and_then(|wanted| {
                    item_candidates.iter().position(|c| &c.product.id == wanted)
                });
                match chosen {
                    Some(pos) => {
                        // Drop every other candidate: the household chose this one,
                        // so it is the only option priced (deterministic, and it
                        // never undercuts the household's explicit choice).
                        let confirmed = item_candidates.swap_remove(pos);
                        candidates.push(confirmed);
                    }
                    None => {
                        // `canonical_item_id` is bound above: an unresolved item
                        // never reaches the policy branch (it short-circuits at 1a).
                        needs_confirmation.push(NeedsConfirmationItem {
                            item_index,
                            entry_id: item.entry_id.clone(),
                            display_text: item.display_text.clone(),
                            canonical_item_id,
                            candidate_count: item_candidates.len(),
                            candidates: candidate_offers(&item_candidates),
                        });
                    }
                }
            }
        }
    }

    (
        candidates,
        Classification {
            unresolved,
            no_valid_offer,
            needs_confirmation,
            category_excluded,
        },
    )
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the optimiser end-to-end against the database.
///
/// Fetches, per resolved item: the `accepted`/`suggested` matches, each matched
/// product's row, and that product's latest offer. Applies the constraint filter
/// and classification, runs the pure core, and layers the recommendation on top.
///
/// For each resolved item it also reads the household's most-recent `prefer`
/// choice (issue #78) so an ask-gated item the household has already confirmed is
/// priced into the plans instead of being held back (see [`classify_items`]).
///
/// Reads run in autocommit on the pool; the optimiser never holds a write lock.
pub async fn optimise(
    pool: &SqlitePool,
    input: &OptimiserInput,
) -> Result<OptimiserOutput, DbError> {
    let mut priced_inputs: BTreeMap<usize, Vec<(ProductMatch, RetailerProduct, ProductOffer)>> =
        BTreeMap::new();
    let mut preferred_products: BTreeMap<usize, String> = BTreeMap::new();

    for (item_index, item) in input.items.iter().enumerate() {
        let Some(canonical_item_id) = item.canonical_item_id.as_deref() else {
            continue; // Unresolved; handled by classify_items.
        };
        let matches =
            db::accepted_or_suggested_matches(pool, &input.household_id, canonical_item_id).await?;
        let mut rows = Vec::new();
        for m in matches {
            let Some(product) = db::retailer_product_by_id(pool, &m.retailer_product_id).await?
            else {
                continue;
            };
            let Some(offer) = db::latest_offer_for_product(pool, &m.retailer_product_id).await?
            else {
                continue;
            };
            rows.push((m, product, offer));
        }
        priced_inputs.insert(item_index, rows);

        // The household's confirmed product for this item, if any (issue #78).
        // `preferred_products` is only consulted by `classify_items` on the `Ask`
        // branch, so gate the per-item SQL on that policy: a basket with no ask
        // items must not pay one `preferred_product_for_item` round-trip per
        // resolved line (the cost would scale linearly with basket size for a
        // result that is then discarded). Default to the permissive `Any` when the
        // item has no constraints row, mirroring `classify_items`.
        let substitution_policy = item
            .constraints
            .as_ref()
            .map(|c| c.substitution_policy)
            .unwrap_or(SubstitutionPolicy::Any);
        if substitution_policy == SubstitutionPolicy::Ask {
            if let Some(preferred) =
                db::preferred_product_for_item(pool, &input.household_id, canonical_item_id).await?
            {
                preferred_products.insert(item_index, preferred);
            }
        }
    }

    Ok(build_output(input, &priced_inputs, &preferred_products))
}

/// The cheapest Aldi price for one basket item from its raw match/offer rows,
/// ignoring store settings and per-store category rules (issue #100).
///
/// This is the counterfactual half of the Aldi comparison: whether Aldi is
/// switched off or its category rules exclude the item, the household still wants
/// to know what Aldi would have charged. Only the item's own constraints are
/// applied (a brand/size rule is about the product, not the store); under
/// `substitution_policy = 'strict'` a merely suggested match is not a valid
/// choice, so it is skipped. Returns the effective unit price, or `None` when
/// Aldi has no qualifying offer.
fn aldi_unit_price(
    input: &OptimiserInput,
    item_index: usize,
    rows: &[(ProductMatch, RetailerProduct, ProductOffer)],
) -> Option<Cents> {
    let item = &input.items[item_index];
    let strict = item
        .constraints
        .as_ref()
        .map(|c| c.substitution_policy)
        .unwrap_or(SubstitutionPolicy::Any)
        == SubstitutionPolicy::Strict;
    let mut best: Option<Candidate> = None;
    for (m, product, offer) in rows {
        if product.retailer != Retailer::Aldi {
            continue;
        }
        match check_constraints(item.constraint_policy, item.constraints.as_ref(), product) {
            ConstraintCheck::Pass => {}
            // A config error already sent the item to `no_valid_offer_items`, so it
            // cannot be in the recommended plan and never reaches this function;
            // reject it here too rather than price an invalid item.
            ConstraintCheck::ConfigError | ConstraintCheck::Reject => continue,
        }
        let accepted = m.status == MatchStatus::Accepted;
        if strict && !accepted {
            continue;
        }
        let cand = Candidate {
            item_index,
            retailer: Retailer::Aldi,
            product: product.clone(),
            offer: offer.clone(),
            accepted,
        };
        if best.as_ref().is_none_or(|b| is_cheaper(&cand, b)) {
            best = Some(cand);
        }
    }
    best.map(|c| c.effective_unit_price())
}

/// Build the Aldi comparison against the recommended plan (issue #100).
///
/// For every item the recommended plan prices, Aldi's cheapest constraint-passing
/// offer is looked up independently of the store-off and category-exclusion rules
/// and totalled per fresh/packaged split. An item Aldi does not price is dropped
/// from both sides, so the two totals always cover the same items. Returns `None`
/// when there is no recommended plan (nothing to compare against).
fn aldi_comparison(
    input: &OptimiserInput,
    priced_inputs: &BTreeMap<usize, Vec<(ProductMatch, RetailerProduct, ProductOffer)>>,
    recommended: Option<&StorePlan>,
) -> Option<AldiComparison> {
    let plan = recommended?;
    let mut packaged = AldiComparisonSplit::default();
    let mut fresh = AldiComparisonSplit::default();
    for line in &plan.lines {
        let Some(rows) = priced_inputs.get(&line.item_index) else {
            continue;
        };
        let Some(unit_price) = aldi_unit_price(input, line.item_index, rows) else {
            continue;
        };
        let aldi_total = unit_price * i64::from(input.items[line.item_index].count);
        let split = if input.items[line.item_index].is_fresh {
            &mut fresh
        } else {
            &mut packaged
        };
        split.item_count += 1;
        split.aldi_total_cents += aldi_total;
        split.recommended_total_cents += line.line_total_cents;
        split.saving_cents += line.line_total_cents - aldi_total;
    }
    Some(AldiComparison { packaged, fresh })
}

/// Build the full [`OptimiserOutput`] from already-fetched per-item rows. Shared
/// by [`optimise`] and the in-memory tests.
fn build_output(
    input: &OptimiserInput,
    priced_inputs: &BTreeMap<usize, Vec<(ProductMatch, RetailerProduct, ProductOffer)>>,
    preferred_products: &BTreeMap<usize, String>,
) -> OptimiserOutput {
    let (candidates, classification) = classify_items(input, priced_inputs, preferred_products);
    // Resolved-but-unpriceable items are missing at every store, so they fold into
    // every plan's missing_count as a constant (see StorePlan::missing_count).
    let no_offer_count = classification.no_valid_offer.len();
    let comp = optimise_core_with_candidates(input, &candidates, no_offer_count);
    let nearby: Vec<Retailer> = Retailer::ALL
        .into_iter()
        .filter(|r| input.is_nearby(*r))
        .collect();
    let decision = decide_recommendation(
        &comp.plans,
        &comp.two_store_candidates,
        input.threshold_2_cents,
        &nearby,
    );
    // Reasons are derived from the same computation that produced the plans, so
    // each card's explanation cannot drift from the lines it describes.
    let reasons = build_reasons(input, &comp, &decision, &classification);
    // The Aldi counterfactual is computed from the raw rows and the recommended
    // plan, after the plans are fixed: it can never influence what is recommended.
    let aldi_comparison = aldi_comparison(
        input,
        priced_inputs,
        decision.recommendation.as_ref().map(|r| &r.plan),
    );
    OptimiserOutput {
        basket_items: input
            .items
            .iter()
            .enumerate()
            .map(|(item_index, item)| BasketItemIdentity {
                item_index,
                entry_id: item.entry_id.clone(),
                display_text: item.display_text.clone(),
            })
            .collect(),
        best_single: comp.plans.best_single,
        best_two_store: comp.plans.best_two_store,
        theoretical_minimum: comp.plans.theoretical_minimum,
        recommended: decision.recommendation,
        unresolved_items: classification.unresolved,
        no_valid_offer_items: classification.no_valid_offer,
        needs_confirmation_items: classification.needs_confirmation,
        reasons,
        aldi_comparison,
    }
}

#[cfg(test)]
mod tests;
