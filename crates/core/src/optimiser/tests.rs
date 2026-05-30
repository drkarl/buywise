//! Tests for the basket optimiser.
//!
//! Two layers:
//!
//! * **In-memory unit tests** drive [`super::build_output`] / [`super::optimise_core`]
//!   with hand-built rows. This is the dominant pattern in the repo (deterministic,
//!   no async, no schema) and covers single-store, two-store, loyalty, freshness,
//!   the convenience threshold boundary, the Dunnes voucher boundaries, brand
//!   rejection, and the brand-config error.
//! * **SQLite integration tests** seed a real in-memory database and run
//!   [`super::optimise`] through the `db.rs` helpers, proving the read path and the
//!   `accepted`/`suggested` SQL filter.

use super::*;
use crate::db;
use crate::domain::{
    MatchStatus, ProductMatch, ProductOffer, Retailer, RetailerProduct, Source, SourceConfidence,
    SubstitutionPolicy,
};
use sqlx::sqlite::SqlitePool;

// ---------------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------------

/// A retailer product with brand/size left unset; only `id`, `retailer`, and
/// `name` matter for the unconstrained tests.
fn product(id: &str, retailer: Retailer) -> RetailerProduct {
    RetailerProduct {
        id: id.into(),
        source: Source::BasketWatch,
        retailer,
        external_id: Some(id.into()),
        url: None,
        name: id.into(),
        brand: None,
        category: None,
        pack_size_text: None,
        size_value: None,
        size_unit: None,
        pack_count: Some(1),
        first_seen_at: "2026-05-01T00:00:00Z".into(),
        last_seen_at: "2026-05-29T00:00:00Z".into(),
    }
}

/// A retailer product carrying a brand and an SI size.
fn branded_product(
    id: &str,
    retailer: Retailer,
    brand: &str,
    size_value: f64,
    size_unit: &str,
) -> RetailerProduct {
    RetailerProduct {
        brand: Some(brand.into()),
        size_value: Some(size_value),
        size_unit: Some(size_unit.into()),
        ..product(id, retailer)
    }
}

/// A minimal offer: shelf price only, run number `at_fetch`.
fn offer(id: &str, product_id: &str, price_cents: Cents, at_fetch: Option<i64>) -> ProductOffer {
    ProductOffer {
        id: id.into(),
        retailer_product_id: product_id.into(),
        price_cents,
        unit_price_eur_micros: None,
        unit_price_basis: None,
        previous_price_cents: None,
        loyalty_price_cents: None,
        loyalty_scheme: None,
        offer_text: None,
        offer_valid_until: None,
        source_snapshot_date: None,
        fetched_at: "2026-05-29T10:00:00Z".into(),
        cache_expires_at: None,
        last_successful_source_run_id: None,
        source_successful_run_number_at_fetch: at_fetch,
        source_confidence_at_ingest: SourceConfidence::High,
    }
}

/// An offer with a loyalty price (Clubcard) on top of a shelf price.
fn loyalty_offer(
    id: &str,
    product_id: &str,
    price_cents: Cents,
    loyalty_cents: Cents,
) -> ProductOffer {
    ProductOffer {
        loyalty_price_cents: Some(loyalty_cents),
        loyalty_scheme: Some("clubcard".into()),
        ..offer(id, product_id, price_cents, Some(5))
    }
}

fn product_match(id: &str, item_id: &str, product_id: &str, status: MatchStatus) -> ProductMatch {
    ProductMatch {
        id: id.into(),
        household_id: "hh-1".into(),
        canonical_item_id: item_id.into(),
        retailer_product_id: product_id.into(),
        status,
        matched_by: "system".into(),
        created_at: "2026-05-01T00:00:00Z".into(),
        updated_at: "2026-05-01T00:00:00Z".into(),
    }
}

/// A resolved basket item with `Any` policy and no constraints.
fn item(display: &str, canonical_id: &str, count: u32) -> BasketItem {
    BasketItem {
        display_text: display.into(),
        canonical_item_id: Some(canonical_id.into()),
        count,
        constraint_policy: ConstraintPolicy::Any,
        constraints: None,
    }
}

/// Build an `ItemConstraints` with just a brand set (used for brand-policy tests).
fn brand_constraints(brand: Option<&str>) -> ItemConstraints {
    ItemConstraints {
        id: "ic-1".into(),
        canonical_item_id: "ci-1".into(),
        brand: brand.map(Into::into),
        allowed_brands: None,
        excluded_brands: None,
        preferred_size_value: None,
        preferred_size_basis: None,
        min_size_value: None,
        min_size_basis: None,
        max_size_value: None,
        max_size_basis: None,
        pack_count: None,
        form_factor: None,
        dietary_flags: None,
        substitution_policy: SubstitutionPolicy::Any,
        updated_at: "2026-05-29T00:00:00Z".into(),
    }
}

type Rows = Vec<(ProductMatch, RetailerProduct, ProductOffer)>;

/// Run the in-memory path: classify + core + recommend.
fn run(input: &OptimiserInput, priced: BTreeMap<usize, Rows>) -> OptimiserOutput {
    build_output(input, &priced)
}

// ---------------------------------------------------------------------------
// Single-store
// ---------------------------------------------------------------------------

#[test]
fn single_store_picks_cheapest_complete_retailer() {
    // One item, two retailers. Tesco 200, Aldi 150. Aldi is cheaper and complete.
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-t", "ci-milk", "p-t", MatchStatus::Accepted),
                product("p-t", Retailer::Tesco),
                offer("o-t", "p-t", 200, Some(5)),
            ),
            (
                product_match("m-a", "ci-milk", "p-a", MatchStatus::Accepted),
                product("p-a", Retailer::Aldi),
                offer("o-a", "p-a", 150, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("a single-store plan");
    assert_eq!(single.stores, vec![Retailer::Aldi]);
    assert_eq!(single.stores.len(), 1, "best_single has exactly one store");
    assert_eq!(single.total_cents, 150);
    assert_eq!(single.missing_count, 0);
}

#[test]
fn line_total_multiplies_by_count() {
    // count = 3 → line total = 3 × unit price.
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 3)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-milk", "p", MatchStatus::Accepted),
            product("p", Retailer::Tesco),
            offer("o", "p", 200, Some(5)),
        )],
    );
    let out = run(&input, priced);
    let single = out.best_single.unwrap();
    assert_eq!(single.total_cents, 600);
    assert_eq!(single.lines[0].unit_price_cents, 200);
    assert_eq!(single.lines[0].line_total_cents, 600);
}

// ---------------------------------------------------------------------------
// Two-store split + invariant
// ---------------------------------------------------------------------------

#[test]
fn two_store_split_and_invariant() {
    // Item A cheapest at Aldi; item B cheapest at Tesco. No single store has both
    // cheapest, so the two-store split beats either single store. Theoretical
    // minimum equals the two-store split here (each item at its global cheapest).
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("rice", "ci-rice", 1), item("coffee", "ci-coffee", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    // rice: Aldi 100 (cheapest), Tesco 130.
    priced.insert(
        0,
        vec![
            (
                product_match("m-ra", "ci-rice", "p-ra", MatchStatus::Accepted),
                product("p-ra", Retailer::Aldi),
                offer("o-ra", "p-ra", 100, Some(5)),
            ),
            (
                product_match("m-rt", "ci-rice", "p-rt", MatchStatus::Accepted),
                product("p-rt", Retailer::Tesco),
                offer("o-rt", "p-rt", 130, Some(5)),
            ),
        ],
    );
    // coffee: Tesco 300 (cheapest), Aldi 360.
    priced.insert(
        1,
        vec![
            (
                product_match("m-ct", "ci-coffee", "p-ct", MatchStatus::Accepted),
                product("p-ct", Retailer::Tesco),
                offer("o-ct", "p-ct", 300, Some(5)),
            ),
            (
                product_match("m-ca", "ci-coffee", "p-ca", MatchStatus::Accepted),
                product("p-ca", Retailer::Aldi),
                offer("o-ca", "p-ca", 360, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("single");
    let two = out.best_two_store.expect("two");
    let theo = out.theoretical_minimum.expect("theo");

    assert_eq!(two.stores.len(), 2, "best_two_store has exactly two stores");
    // Best single: Tesco (130 + 300 = 430) vs Aldi (100 + 360 = 460) → Tesco 430.
    assert_eq!(single.total_cents, 430);
    // Two-store: rice@Aldi 100 + coffee@Tesco 300 = 400.
    assert_eq!(two.total_cents, 400);
    // Theoretical minimum: same as two-store here.
    assert_eq!(theo.total_cents, 400);

    // The acceptance-criteria invariant among complete plans.
    assert_eq!(single.missing_count, 0);
    assert_eq!(two.missing_count, 0);
    assert_eq!(theo.missing_count, 0);
    assert!(theo.total_cents <= two.total_cents);
    assert!(two.total_cents <= single.total_cents);
}

// ---------------------------------------------------------------------------
// Loyalty price application
// ---------------------------------------------------------------------------

#[test]
fn loyalty_price_is_applied_and_flagged() {
    // Tesco shelf 250 but Clubcard 200; Aldi shelf 220, no loyalty. With loyalty,
    // Tesco (200) beats Aldi (220).
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("fairy", "ci-fairy", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-t", "ci-fairy", "p-t", MatchStatus::Accepted),
                product("p-t", Retailer::Tesco),
                loyalty_offer("o-t", "p-t", 250, 200),
            ),
            (
                product_match("m-a", "ci-fairy", "p-a", MatchStatus::Accepted),
                product("p-a", Retailer::Aldi),
                offer("o-a", "p-a", 220, Some(5)),
            ),
        ],
    );
    let out = run(&input, priced);
    let single = out.best_single.unwrap();
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(
        single.total_cents, 200,
        "loyalty price used, not shelf price"
    );
    assert!(single.lines[0].loyalty_applied);
}

// ---------------------------------------------------------------------------
// Freshness confidence
// ---------------------------------------------------------------------------

#[test]
fn freshness_confidence_levels() {
    // current run = 5. at_fetch 5 → High, 4 → Low, 3 → Unknown.
    assert_eq!(
        freshness_confidence(Some(5), Some(5)),
        FreshnessConfidence::High
    );
    assert_eq!(
        freshness_confidence(Some(5), Some(4)),
        FreshnessConfidence::Low
    );
    assert_eq!(
        freshness_confidence(Some(5), Some(3)),
        FreshnessConfidence::Unknown
    );
    // NULL on either side → Unknown.
    assert_eq!(
        freshness_confidence(None, Some(5)),
        FreshnessConfidence::Unknown
    );
    assert_eq!(
        freshness_confidence(Some(5), None),
        FreshnessConfidence::Unknown
    );
    assert_eq!(
        freshness_confidence(None, None),
        FreshnessConfidence::Unknown
    );
}

#[test]
fn freshness_unknown_when_current_run_is_null() {
    // Acceptance: Unknown when current_source_run_number IS NULL.
    let input = OptimiserInput::new("hh-1", None, vec![item("milk", "ci-milk", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-milk", "p", MatchStatus::Accepted),
            product("p", Retailer::Tesco),
            offer("o", "p", 200, Some(5)),
        )],
    );
    let out = run(&input, priced);
    let single = out.best_single.unwrap();
    assert_eq!(
        single.lines[0].freshness_confidence,
        FreshnessConfidence::Unknown
    );
}

// ---------------------------------------------------------------------------
// Convenience threshold boundary
// ---------------------------------------------------------------------------

/// Build a two-item, two-retailer basket where the two-store split saves exactly
/// `saving_cents` over the best single store, then return the recommendation.
///
/// Layout (both stores individually complete, so completeness never decides):
/// * rice:   Tesco 100,             Aldi 100 + saving  → Tesco cheaper
/// * coffee: Tesco 300,             Aldi 300 - saving  → Aldi cheaper
///
/// Best single store: Tesco = 400 (Aldi also totals 400). The two-store split
/// takes rice@Tesco + coffee@Aldi = 400 - saving, genuinely using both stores,
/// so `single.total - two.total = saving`.
fn recommendation_with_saving(saving_cents: Cents) -> Recommendation {
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("rice", "ci-rice", 1), item("coffee", "ci-coffee", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-rt", "ci-rice", "p-rt", MatchStatus::Accepted),
                product("p-rt", Retailer::Tesco),
                offer("o-rt", "p-rt", 100, Some(5)),
            ),
            (
                product_match("m-ra", "ci-rice", "p-ra", MatchStatus::Accepted),
                product("p-ra", Retailer::Aldi),
                offer("o-ra", "p-ra", 100 + saving_cents, Some(5)),
            ),
        ],
    );
    priced.insert(
        1,
        vec![
            (
                product_match("m-ct", "ci-coffee", "p-ct", MatchStatus::Accepted),
                product("p-ct", Retailer::Tesco),
                offer("o-ct", "p-ct", 300, Some(5)),
            ),
            (
                product_match("m-ca", "ci-coffee", "p-ca", MatchStatus::Accepted),
                product("p-ca", Retailer::Aldi),
                offer("o-ca", "p-ca", 300 - saving_cents, Some(5)),
            ),
        ],
    );
    run(&input, priced).recommended.expect("recommendation")
}

#[test]
fn convenience_threshold_below_ten_euro_does_not_clear() {
    // Saving of €9.99 (999c) < €10 threshold → single store recommended.
    let rec = recommendation_with_saving(999);
    assert_eq!(rec.kind, RecommendedKind::SingleStore);
    assert!(!rec.clears_convenience_threshold);
    assert_eq!(rec.two_store_saving_cents, 999);
}

#[test]
fn convenience_threshold_exactly_ten_euro_does_not_clear() {
    // Saving of exactly €10 (1000c) is NOT strictly greater than threshold →
    // single store. The threshold is a strict ">".
    let rec = recommendation_with_saving(1000);
    assert_eq!(rec.kind, RecommendedKind::SingleStore);
    assert!(!rec.clears_convenience_threshold);
}

#[test]
fn convenience_threshold_above_ten_euro_clears() {
    // Saving of €10.01 (1001c) > €10 → two-store recommended.
    let rec = recommendation_with_saving(1001);
    assert_eq!(rec.kind, RecommendedKind::TwoStore);
    assert!(rec.clears_convenience_threshold);
    assert_eq!(rec.two_store_saving_cents, 1001);
}

// ---------------------------------------------------------------------------
// Dunnes voucher boundary
// ---------------------------------------------------------------------------

/// Single Dunnes item priced at exactly `price_cents`; returns the plan's voucher
/// text (if any).
fn dunnes_voucher_for_basket(price_cents: Cents) -> Option<String> {
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("hamper", "ci-hamper", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-hamper", "p", MatchStatus::Accepted),
            product("p", Retailer::Dunnes),
            offer("o", "p", price_cents, Some(5)),
        )],
    );
    run(&input, priced)
        .best_single
        .unwrap()
        .dunnes_voucher_offer_text
}

#[test]
fn dunnes_voucher_boundaries() {
    // 44.99 → no flag (just below window).
    assert_eq!(dunnes_voucher_for_basket(4499), None);
    // 45.00 → flag, add €5.00.
    assert_eq!(
        dunnes_voucher_for_basket(4500),
        Some("add €5.00 more to potentially trigger the €50 voucher".into())
    );
    // 49.99 → flag, add €0.01.
    assert_eq!(
        dunnes_voucher_for_basket(4999),
        Some("add €0.01 more to potentially trigger the €50 voucher".into())
    );
    // 50.00 → flag, add €0.00 (inclusive upper bound).
    assert_eq!(
        dunnes_voucher_for_basket(5000),
        Some("add €0.00 more to potentially trigger the €50 voucher".into())
    );
    // 50.01 → no flag (just above window).
    assert_eq!(dunnes_voucher_for_basket(5001), None);
}

#[test]
fn dunnes_voucher_only_counts_dunnes_lines() {
    // A Tesco line at €48 must NOT trigger the Dunnes voucher.
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("hamper", "ci-hamper", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-hamper", "p", MatchStatus::Accepted),
            product("p", Retailer::Tesco),
            offer("o", "p", 4800, Some(5)),
        )],
    );
    assert_eq!(
        run(&input, priced)
            .best_single
            .unwrap()
            .dunnes_voucher_offer_text,
        None
    );
}

// ---------------------------------------------------------------------------
// Brand constraint rejection + config error
// ---------------------------------------------------------------------------

#[test]
fn brand_constraint_rejects_own_label() {
    // policy=brand, brand=Fairy. Two candidates: Fairy (433ml) and own-label
    // (Tesco brand). Only Fairy survives.
    let mut basket_item = item("fairy washing up liquid", "ci-fairy", 1);
    basket_item.constraint_policy = ConstraintPolicy::Brand;
    basket_item.constraints = Some(brand_constraints(Some("Fairy")));
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-f", "ci-fairy", "p-fairy", MatchStatus::Accepted),
                branded_product("p-fairy", Retailer::Tesco, "Fairy", 433.0, "ml"),
                offer("o-f", "p-fairy", 250, Some(5)),
            ),
            (
                product_match("m-o", "ci-fairy", "p-own", MatchStatus::Suggested),
                branded_product("p-own", Retailer::Tesco, "Tesco", 500.0, "ml"),
                // Cheaper own-label that must be excluded by the brand constraint.
                offer("o-o", "p-own", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("Fairy is a valid candidate");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-fairy");
    assert_eq!(
        single.total_cents, 250,
        "own-label (100) excluded; only Fairy (250) priced"
    );
    assert!(out.no_valid_offer_items.is_empty());
}

#[test]
fn brand_policy_with_null_brand_is_config_error() {
    // policy=brand but brand IS NULL → no_valid_offer_items with the config-error
    // reason, NOT unresolved, and never silently falls back to `any`.
    let mut basket_item = item("mystery brand item", "ci-x", 1);
    basket_item.constraint_policy = ConstraintPolicy::Brand;
    basket_item.constraints = Some(brand_constraints(None));
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-x", "p", MatchStatus::Accepted),
            branded_product("p", Retailer::Tesco, "AnyBrand", 500.0, "ml"),
            offer("o", "p", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    assert!(out.best_single.is_none(), "no plan: item was rejected");
    assert!(
        out.unresolved_items.is_empty(),
        "config error is not unresolved"
    );
    assert_eq!(out.no_valid_offer_items.len(), 1);
    assert_eq!(
        out.no_valid_offer_items[0].reason,
        NoOfferReason::BrandPolicyMissingBrand
    );
}

#[test]
fn brand_constraint_eliminating_all_is_no_valid_offer_not_unresolved() {
    // policy=brand=Fairy but only an own-label candidate exists → all filtered
    // out → no_valid_offer_items (ConstraintsEliminatedAll), not unresolved.
    let mut basket_item = item("fairy", "ci-fairy", 1);
    basket_item.constraint_policy = ConstraintPolicy::Brand;
    basket_item.constraints = Some(brand_constraints(Some("Fairy")));
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-fairy", "p", MatchStatus::Accepted),
            branded_product("p", Retailer::Tesco, "Tesco", 500.0, "ml"),
            offer("o", "p", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    assert!(out.best_single.is_none());
    assert!(out.unresolved_items.is_empty());
    assert_eq!(out.no_valid_offer_items.len(), 1);
    assert_eq!(
        out.no_valid_offer_items[0].reason,
        NoOfferReason::ConstraintsEliminatedAll
    );
}

// ---------------------------------------------------------------------------
// Size band
// ---------------------------------------------------------------------------

#[test]
fn size_band_accepts_within_20_percent_and_rejects_outside() {
    // Preferred 500ml. 600ml is exactly +20% (500 × 1.20), the INCLUSIVE upper
    // edge of the band, so it must pass; 700ml is +40% (rejected).
    let within = branded_product("p1", Retailer::Tesco, "Fairy", 600.0, "ml");
    let outside = branded_product("p2", Retailer::Tesco, "Fairy", 700.0, "ml");
    assert!(super::size_within_band(
        &within,
        500.0,
        SizeBasis::Millilitres
    ));
    assert!(!super::size_within_band(
        &outside,
        500.0,
        SizeBasis::Millilitres
    ));
    // Different basis (grams vs ml) is rejected.
    let wrong_basis = branded_product("p3", Retailer::Tesco, "Fairy", 500.0, "g");
    assert!(!super::size_within_band(
        &wrong_basis,
        500.0,
        SizeBasis::Millilitres
    ));
}

// ---------------------------------------------------------------------------
// Unresolved + partial totals
// ---------------------------------------------------------------------------

#[test]
fn unresolved_items_excluded_from_total() {
    // Item 0 resolved and priced; item 1 unresolved (no canonical id).
    let unresolved = BasketItem {
        display_text: "yoghurt".into(),
        canonical_item_id: None,
        count: 1,
        constraint_policy: ConstraintPolicy::Any,
        constraints: None,
    };
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), unresolved],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-milk", "p", MatchStatus::Accepted),
            product("p", Retailer::Tesco),
            offer("o", "p", 200, Some(5)),
        )],
    );
    let out = run(&input, priced);
    assert_eq!(out.unresolved_items.len(), 1);
    assert_eq!(out.unresolved_items[0].item_index, 1);
    assert_eq!(out.unresolved_items[0].display_text, "yoghurt");
    // Total reflects only the resolved item.
    assert_eq!(out.best_single.unwrap().total_cents, 200);
}

#[test]
fn item_with_no_matches_is_no_valid_offer() {
    // Resolved item but no rows at all → NoMatches.
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let priced: BTreeMap<usize, Rows> = BTreeMap::new(); // no entry for index 0
    let out = run(&input, priced);
    assert!(out.best_single.is_none());
    assert_eq!(out.no_valid_offer_items.len(), 1);
    assert_eq!(out.no_valid_offer_items[0].reason, NoOfferReason::NoMatches);
}

#[test]
fn missing_count_includes_resolved_but_unpriceable_items() {
    // Two resolved items. Item 0 is priced at Tesco; item 1 resolves but has no
    // matches anywhere (→ no_valid_offer_items). The plan must report
    // missing_count == 1 (the unpriceable line is missing at every store), never
    // 0, so the field stays honest about basket completeness.
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("rice", "ci-rice", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-milk", "p", MatchStatus::Accepted),
            product("p", Retailer::Tesco),
            offer("o", "p", 200, Some(5)),
        )],
    );
    // No entry for index 1 → resolved item with no offer → NoMatches.

    let out = run(&input, priced);
    assert_eq!(out.no_valid_offer_items.len(), 1);
    let single = out.best_single.expect("milk is priceable");
    assert_eq!(single.lines.len(), 1, "only milk is priced");
    assert_eq!(single.total_cents, 200);
    assert_eq!(
        single.missing_count, 1,
        "the resolved-but-unpriceable rice counts as missing in the plan"
    );
    // Theoretical minimum carries the same honest missing_count.
    assert_eq!(out.theoretical_minimum.expect("theo").missing_count, 1);
}

// ---------------------------------------------------------------------------
// Fulfilment rule: missing-count beats cost
// ---------------------------------------------------------------------------

#[test]
fn fulfilment_rule_completeness_beats_cost() {
    // Two items. Aldi has only item 0 (cheap). Tesco has both. The single-store
    // ranking must prefer Tesco (0 missing) over Aldi (1 missing) even though
    // Aldi's priced subset is cheaper.
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("rice", "ci-rice", 1), item("coffee", "ci-coffee", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-ra", "ci-rice", "p-ra", MatchStatus::Accepted),
                product("p-ra", Retailer::Aldi),
                offer("o-ra", "p-ra", 50, Some(5)),
            ),
            (
                product_match("m-rt", "ci-rice", "p-rt", MatchStatus::Accepted),
                product("p-rt", Retailer::Tesco),
                offer("o-rt", "p-rt", 200, Some(5)),
            ),
        ],
    );
    // coffee only at Tesco.
    priced.insert(
        1,
        vec![(
            product_match("m-ct", "ci-coffee", "p-ct", MatchStatus::Accepted),
            product("p-ct", Retailer::Tesco),
            offer("o-ct", "p-ct", 300, Some(5)),
        )],
    );
    let out = run(&input, priced);
    let single = out.best_single.unwrap();
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(single.missing_count, 0);
    assert_eq!(single.total_cents, 500);
}

// ---------------------------------------------------------------------------
// Accepted is a tie-breaker only
// ---------------------------------------------------------------------------

#[test]
fn accepted_breaks_price_ties_only() {
    // Same retailer, same price (200): accepted match wins over suggested.
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-sug", "ci-milk", "p-sug", MatchStatus::Suggested),
                product("p-sug", Retailer::Tesco),
                offer("o-sug", "p-sug", 200, Some(5)),
            ),
            (
                product_match("m-acc", "ci-milk", "p-acc", MatchStatus::Accepted),
                product("p-acc", Retailer::Tesco),
                offer("o-acc", "p-acc", 200, Some(5)),
            ),
        ],
    );
    let out = run(&input, priced);
    let single = out.best_single.unwrap();
    assert_eq!(single.lines[0].retailer_product_id, "p-acc");
}

// ---------------------------------------------------------------------------
// Pure core entry point
// ---------------------------------------------------------------------------

#[test]
fn optimise_core_pure_entry_point_returns_three_plans() {
    // Exercise the documented pure entry point directly with hand-built
    // candidates (no classification, no DB). One item at Aldi 100 / Tesco 130.
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let candidates = vec![
        Candidate {
            item_index: 0,
            retailer: Retailer::Aldi,
            product: product("p-a", Retailer::Aldi),
            offer: offer("o-a", "p-a", 100, Some(5)),
            accepted: true,
        },
        Candidate {
            item_index: 0,
            retailer: Retailer::Tesco,
            product: product("p-t", Retailer::Tesco),
            offer: offer("o-t", "p-t", 130, Some(5)),
            accepted: true,
        },
    ];
    let core = optimise_core(&input, &candidates, 0);
    assert_eq!(core.best_single.expect("single").total_cents, 100);
    // A single item cannot fill a GENUINE two-store plan: every candidate pair
    // degenerates to one store, so best_two_store is None (the two-store plan
    // must span exactly two distinct retailers).
    assert!(
        core.best_two_store.is_none(),
        "a single priceable item cannot yield a two-store split"
    );
    assert_eq!(core.theoretical_minimum.expect("theo").total_cents, 100);
}

#[test]
fn best_two_store_is_none_when_one_store_dominates_a_multi_retailer_basket() {
    // Regression for the degeneracy bug: a >=2-retailer basket where Tesco wins
    // (or solely stocks) every item. Each candidate pair collapses to a single
    // store, so there is NO genuine two-store split → best_two_store is None,
    // never a one-store plan masquerading as two-store. best_single still works.
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("rice", "ci-rice", 1), item("coffee", "ci-coffee", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    // rice: Tesco 100 (cheaper), Aldi 200.
    priced.insert(
        0,
        vec![
            (
                product_match("m-rt", "ci-rice", "p-rt", MatchStatus::Accepted),
                product("p-rt", Retailer::Tesco),
                offer("o-rt", "p-rt", 100, Some(5)),
            ),
            (
                product_match("m-ra", "ci-rice", "p-ra", MatchStatus::Accepted),
                product("p-ra", Retailer::Aldi),
                offer("o-ra", "p-ra", 200, Some(5)),
            ),
        ],
    );
    // coffee: Tesco 300 (cheaper), Aldi 400. Tesco wins everything.
    priced.insert(
        1,
        vec![
            (
                product_match("m-ct", "ci-coffee", "p-ct", MatchStatus::Accepted),
                product("p-ct", Retailer::Tesco),
                offer("o-ct", "p-ct", 300, Some(5)),
            ),
            (
                product_match("m-ca", "ci-coffee", "p-ca", MatchStatus::Accepted),
                product("p-ca", Retailer::Aldi),
                offer("o-ca", "p-ca", 400, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("single");
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(single.total_cents, 400);
    assert!(
        out.best_two_store.is_none(),
        "no genuine two-store split exists; best_two_store must be None, not a one-store plan"
    );
    // The recommendation falls back to the single store.
    let rec = out.recommended.expect("recommendation");
    assert_eq!(rec.kind, RecommendedKind::SingleStore);
}

// ---------------------------------------------------------------------------
// Fixture schema guard
// ---------------------------------------------------------------------------

/// Every key permitted inside an `item_constraints` fixture object — exactly the
/// columns of the `item_constraints` table / fields of [`ItemConstraints`]. This
/// is the contract the fixture is checked against: any other key (e.g. the
/// removed `constraint_policy`) is drift and must fail the guard test.
const ITEM_CONSTRAINTS_FIELDS: &[&str] = &[
    "id",
    "canonical_item_id",
    "brand",
    "allowed_brands",
    "excluded_brands",
    "preferred_size_value",
    "preferred_size_basis",
    "min_size_value",
    "min_size_basis",
    "max_size_value",
    "max_size_basis",
    "pack_count",
    "form_factor",
    "dietary_flags",
    "substitution_policy",
    "updated_at",
];

#[test]
fn item_constraints_fixture_parses_into_struct() {
    // Guard against schema drift in fixtures/item_constraints.json. Two checks,
    // because `serde` ignores unknown fields by default and `ItemConstraints`
    // deliberately does NOT set `deny_unknown_fields` (it is JSON-round-tripped
    // and a future P1 path may read it from tolerant DB JSON columns):
    //   1. Deserialising into `Vec<ItemConstraints>` proves every required field
    //      is present and correctly typed.
    //   2. Comparing each object's raw key set against ITEM_CONSTRAINTS_FIELDS
    //      proves there are NO extra keys — this is what catches a re-introduced
    //      `constraint_policy` (or any other bogus field) that step 1 would
    //      silently swallow.
    #[derive(serde::Deserialize)]
    struct Fixture {
        item_constraints: Vec<ItemConstraints>,
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/item_constraints.json"
    );
    let bytes = std::fs::read(path).expect("read fixtures/item_constraints.json");

    // Check 2: no unexpected keys in any constraint object.
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).expect("parse fixtures/item_constraints.json as JSON");
    let raw_entries = raw["item_constraints"]
        .as_array()
        .expect("item_constraints must be a JSON array");
    for (i, entry) in raw_entries.iter().enumerate() {
        let obj = entry
            .as_object()
            .unwrap_or_else(|| panic!("item_constraints[{i}] must be a JSON object"));
        for key in obj.keys() {
            assert!(
                ITEM_CONSTRAINTS_FIELDS.contains(&key.as_str()),
                "item_constraints[{i}] has unexpected field {key:?}; \
                 allowed fields are {ITEM_CONSTRAINTS_FIELDS:?} \
                 (did a removed field like constraint_policy come back?)"
            );
        }
    }

    // Check 1: required fields present and well-typed.
    let fixture: Fixture =
        serde_json::from_slice(&bytes).expect("parse fixtures/item_constraints.json");
    assert_eq!(fixture.item_constraints.len(), 2);

    // The Fairy brand example (washing-up liquid → canonical 'dish soap').
    let fairy = &fixture.item_constraints[0];
    assert_eq!(fairy.brand.as_deref(), Some("Fairy"));
    assert_eq!(
        fairy.canonical_item_id, "15a8975bca6f4dd1605c2bf19c1ca5f60894c3ff117bc18d0e75a1e8974cfe4c",
        "must reference the real hashed 'dish soap' canonical id"
    );
    assert_eq!(fairy.preferred_size_value, Some(500.0));
    assert_eq!(fairy.preferred_size_basis, Some(SizeBasis::Millilitres));

    // The whole-milk size example references the real hashed canonical id.
    let milk = &fixture.item_constraints[1];
    assert_eq!(
        milk.canonical_item_id, "dc26ae0a7a60eec7c71f24059c6fbca2542374bf5a49366ff206bb7419fc1559",
        "must reference the real hashed 'whole milk' canonical id"
    );
    assert!(milk.brand.is_none());
}

// ---------------------------------------------------------------------------
// SQLite integration
// ---------------------------------------------------------------------------

async fn fresh_pool() -> SqlitePool {
    let pool = db::connect("sqlite::memory:").await.expect("connect");
    db::run_migrations(&pool).await.expect("migrate");
    pool
}

/// The seeded "Dairy" category UUID (migrations/002_seed.sql). Used so the
/// canonical-item FK to `categories(id)` is satisfied.
const DAIRY_CATEGORY_ID: &str = "c1000000-0000-0000-0000-000000000003";

/// Seed a canonical item against the seeded Dairy category so FK constraints on
/// product_matches/canonical_items are satisfied.
async fn seed_canonical_item(pool: &SqlitePool, id: &str) {
    sqlx::query(
        "INSERT INTO canonical_items \
         (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
         VALUES (?1, 'hh-1', 'Milk', 'Leche', ?2, 'any', '2026-05-01T00:00:00Z')",
    )
    .bind(id)
    .bind(DAIRY_CATEGORY_ID)
    .execute(pool)
    .await
    .expect("seed canonical item");
}

async fn insert_product(pool: &SqlitePool, p: &RetailerProduct) {
    let mut conn = pool.acquire().await.expect("acquire");
    db::upsert_retailer_product(&mut conn, p)
        .await
        .expect("insert product");
}

async fn insert_offer(pool: &SqlitePool, o: &ProductOffer) {
    let mut conn = pool.acquire().await.expect("acquire");
    db::insert_product_offer(&mut conn, o)
        .await
        .expect("insert offer");
}

async fn insert_match(pool: &SqlitePool, m: &ProductMatch) {
    sqlx::query(
        "INSERT INTO product_matches \
         (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&m.id)
    .bind(&m.household_id)
    .bind(&m.canonical_item_id)
    .bind(&m.retailer_product_id)
    .bind(m.status)
    .bind(&m.matched_by)
    .bind(&m.created_at)
    .bind(&m.updated_at)
    .execute(pool)
    .await
    .expect("insert match");
}

#[tokio::test]
async fn integration_optimise_reads_through_pool() {
    let pool = fresh_pool().await;
    seed_canonical_item(&pool, "ci-milk").await;

    // Tesco product + offer (cheaper), Aldi product + offer (dearer).
    insert_product(&pool, &product("p-t", Retailer::Tesco)).await;
    insert_product(&pool, &product("p-a", Retailer::Aldi)).await;
    insert_offer(&pool, &offer("o-t", "p-t", 150, Some(5))).await;
    insert_offer(&pool, &offer("o-a", "p-a", 200, Some(5))).await;
    insert_match(
        &pool,
        &product_match("m-t", "ci-milk", "p-t", MatchStatus::Accepted),
    )
    .await;
    insert_match(
        &pool,
        &product_match("m-a", "ci-milk", "p-a", MatchStatus::Accepted),
    )
    .await;

    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let out = optimise(&pool, &input).await.expect("optimise");
    let single = out.best_single.expect("plan");
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(single.total_cents, 150);
}

#[tokio::test]
async fn integration_latest_offer_wins_and_rejected_match_excluded() {
    let pool = fresh_pool().await;
    seed_canonical_item(&pool, "ci-milk").await;
    insert_product(&pool, &product("p-t", Retailer::Tesco)).await;

    // Two offers for the same product: the later fetched_at (250) is the latest,
    // overriding the earlier (150). The optimiser must use 250.
    let mut early = offer("o-early", "p-t", 150, Some(4));
    early.fetched_at = "2026-05-01T00:00:00Z".into();
    let mut late = offer("o-late", "p-t", 250, Some(5));
    late.fetched_at = "2026-05-29T00:00:00Z".into();
    insert_offer(&pool, &early).await;
    insert_offer(&pool, &late).await;

    // A rejected match for a second product must be ignored by the SQL filter.
    insert_product(&pool, &product("p-a", Retailer::Aldi)).await;
    insert_offer(&pool, &offer("o-a", "p-a", 50, Some(5))).await;
    insert_match(
        &pool,
        &product_match("m-a", "ci-milk", "p-a", MatchStatus::Rejected),
    )
    .await;
    insert_match(
        &pool,
        &product_match("m-t", "ci-milk", "p-t", MatchStatus::Accepted),
    )
    .await;

    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let out = optimise(&pool, &input).await.expect("optimise");
    let single = out.best_single.expect("plan");
    // Aldi's cheap 50 is excluded (rejected match); Tesco's latest 250 is used.
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(single.total_cents, 250);
    // Freshness: current 5, at_fetch 5 → High.
    assert_eq!(
        single.lines[0].freshness_confidence,
        FreshnessConfidence::High
    );
}

#[tokio::test]
async fn integration_current_source_run_number_helper() {
    let pool = fresh_pool().await;
    // No successful run yet → None.
    assert_eq!(
        db::current_source_run_number(&pool, Source::BasketWatch)
            .await
            .expect("run number"),
        None
    );
}
