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
    StoreStatus, SubstitutionPolicy,
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
        form_factor: None,
        dietary_flags: None,
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
        entry_id: None,
        display_text: display.into(),
        canonical_item_id: Some(canonical_id.into()),
        count,
        constraint_policy: ConstraintPolicy::Any,
        constraints: None,
        category_id: None,
        subcategory_id: None,
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

/// An empty `ItemConstraints` (all fields unset, `Any` substitution policy).
/// Callers set just the field(s) under test, mirroring `brand_constraints`.
fn empty_constraints() -> ItemConstraints {
    ItemConstraints {
        id: "ic-1".into(),
        canonical_item_id: "ci-1".into(),
        brand: None,
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

/// A retailer product carrying a brand only (no size), for brand-list filters.
fn brand_only_product(id: &str, retailer: Retailer, brand: &str) -> RetailerProduct {
    RetailerProduct {
        brand: Some(brand.into()),
        ..product(id, retailer)
    }
}

type Rows = Vec<(ProductMatch, RetailerProduct, ProductOffer)>;

/// Run the in-memory path: classify + core + recommend. No confirmed `prefer`
/// choices (the common case); use [`run_with_preferred`] to inject them.
fn run(input: &OptimiserInput, priced: BTreeMap<usize, Rows>) -> OptimiserOutput {
    build_output(input, &priced, &BTreeMap::new())
}

/// Like [`run`], but with a per-item-index → confirmed `retailer_product_id` map
/// (issue #78), mirroring what `optimise` derives from a persisted `prefer`.
fn run_with_preferred(
    input: &OptimiserInput,
    priced: BTreeMap<usize, Rows>,
    preferred: BTreeMap<usize, String>,
) -> OptimiserOutput {
    build_output(input, &priced, &preferred)
}

#[test]
fn output_echoes_all_basket_item_identities_in_input_order() {
    let mut milk = item("milk", "ci-milk", 1);
    milk.entry_id = Some("entry-milk".into());
    let mut bread = item("bread", "ci-bread", 1);
    bread.entry_id = Some("entry-bread".into());
    let input = OptimiserInput::new("hh-1", Some(5), vec![milk, bread]);

    let output = run(&input, BTreeMap::new());

    assert_eq!(
        output.basket_items,
        vec![
            BasketItemIdentity {
                item_index: 0,
                entry_id: Some("entry-milk".into()),
                display_text: "milk".into(),
            },
            BasketItemIdentity {
                item_index: 1,
                entry_id: Some("entry-bread".into()),
                display_text: "bread".into(),
            },
        ]
    );
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
    let single = out.best_single.as_ref().unwrap();
    assert_eq!(
        single.lines[0].freshness_confidence,
        FreshnessConfidence::Unknown
    );
    assert!(reason_with(&out, PlanKind::BestSingle, ReasonCode::StalePrices).is_none());
    let freshness = reason_with(&out, PlanKind::BestSingle, ReasonCode::PriceAsOf)
        .expect("freshness reason without a snapshot date");
    assert_eq!(freshness.snapshot_date, None);
}

#[test]
fn older_unknown_freshness_with_known_run_numbers_is_stale() {
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-milk", "p", MatchStatus::Accepted),
            product("p", Retailer::Tesco),
            offer("o", "p", 200, Some(3)),
        )],
    );

    let out = run(&input, priced);
    let single = out.best_single.as_ref().expect("single-store plan");
    assert_eq!(
        single.lines[0].freshness_confidence,
        FreshnessConfidence::Unknown
    );
    let stale = reason_with(&out, PlanKind::BestSingle, ReasonCode::StalePrices)
        .expect("known older fetch run should be stale");
    assert_eq!(stale.item_indices, vec![0]);
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
    recommendation_with_saving_and_settings(saving_cents, Vec::new())
}

/// The two-item Tesco/Aldi basket used by the convenience-threshold tests, with
/// an explicit household threshold and store settings (issue #98).
fn two_store_split_input(
    threshold_2_cents: Cents,
    store_settings: Vec<StoreSetting>,
) -> OptimiserInput {
    let mut input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("rice", "ci-rice", 1), item("coffee", "ci-coffee", 1)],
    );
    input.threshold_2_cents = threshold_2_cents;
    input.store_settings = store_settings;
    input
}

/// Priced rows for the two-item split where the two-store plan saves exactly
/// `saving_cents` over the best single store (see [`recommendation_with_saving`]).
fn two_store_split_rows(saving_cents: Cents) -> BTreeMap<usize, Rows> {
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
    priced
}

/// Two-item prices keyed by retailer, used to exercise competing two-store pairs.
fn two_item_retailer_rows(prices: &[(Retailer, Cents, Cents)]) -> BTreeMap<usize, Rows> {
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    for (retailer, first_price, second_price) in prices {
        for (index, (canonical_id, price)) in
            [("ci-rice", *first_price), ("ci-coffee", *second_price)]
                .into_iter()
                .enumerate()
        {
            let product_id = format!("p-{retailer:?}-{index}");
            priced.entry(index).or_default().push((
                product_match(
                    &format!("m-{retailer:?}-{index}"),
                    canonical_id,
                    &product_id,
                    MatchStatus::Accepted,
                ),
                product(&product_id, *retailer),
                offer(
                    &format!("o-{retailer:?}-{index}"),
                    &product_id,
                    price,
                    Some(5),
                ),
            ));
        }
    }
    priced
}

/// Like [`recommendation_with_saving`], but with household store settings
/// (issue #98) applied so callers can exercise the nearby/status rules.
fn recommendation_with_saving_and_settings(
    saving_cents: Cents,
    store_settings: Vec<StoreSetting>,
) -> Recommendation {
    run(
        &two_store_split_input(DEFAULT_THRESHOLD_2_CENTS, store_settings),
        two_store_split_rows(saving_cents),
    )
    .recommended
    .expect("recommendation")
}

/// A [`StoreSetting`] with the given status/nearby (issue #98).
fn store_setting(retailer: Retailer, status: StoreStatus, nearby: bool) -> StoreSetting {
    StoreSetting {
        retailer,
        status,
        nearby,
        category_rules: Vec::new(),
    }
}

/// A single Dunnes item priced at exactly `price_cents`, as a one-entry priced
/// map (used for the Dunnes status/voucher tests).
fn dunnes_rows(price_cents: Cents) -> BTreeMap<usize, Rows> {
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-hamper", "p", MatchStatus::Accepted),
            product("p", Retailer::Dunnes),
            offer("o", "p", price_cents, Some(5)),
        )],
    );
    priced
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
// Backlog-T1: full field constraints (allowed/excluded brands, pack_count,
// form_factor, dietary_flags)
// ---------------------------------------------------------------------------

#[test]
fn allowed_brands_excludes_products_not_in_allow_list() {
    // allowed_brands = [Avonmore, Glenisk]. Avonmore survives; Generic is excluded.
    let mut basket_item = item("milk", "ci-milk", 1);
    let mut c = empty_constraints();
    c.allowed_brands = Some(vec!["Avonmore".into(), "Glenisk".into()]);
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-a", "ci-milk", "p-avon", MatchStatus::Accepted),
                brand_only_product("p-avon", Retailer::Tesco, "Avonmore"),
                offer("o-a", "p-avon", 200, Some(5)),
            ),
            (
                product_match("m-g", "ci-milk", "p-gen", MatchStatus::Suggested),
                brand_only_product("p-gen", Retailer::Tesco, "Generic"),
                offer("o-g", "p-gen", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("Avonmore is allowed");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-avon");
    assert_eq!(single.total_cents, 200, "cheaper Generic excluded");
}

#[test]
fn excluded_brands_rejects_blocked_brand() {
    // excluded_brands = [Generic]. Generic is rejected even though it is cheaper.
    let mut basket_item = item("milk", "ci-milk", 1);
    let mut c = empty_constraints();
    c.excluded_brands = Some(vec!["Generic".into()]);
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-a", "ci-milk", "p-avon", MatchStatus::Accepted),
                brand_only_product("p-avon", Retailer::Tesco, "Avonmore"),
                offer("o-a", "p-avon", 200, Some(5)),
            ),
            (
                product_match("m-g", "ci-milk", "p-gen", MatchStatus::Suggested),
                brand_only_product("p-gen", Retailer::Tesco, "Generic"),
                offer("o-g", "p-gen", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("Avonmore survives the block-list");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-avon");
    assert_eq!(single.total_cents, 200, "blocked Generic excluded");
}

#[test]
fn pack_count_filters_to_exact_pack() {
    // pack_count = 4. The 4-pack survives; the 6-pack is rejected.
    let mut basket_item = item("yoghurt", "ci-yog", 1);
    let mut c = empty_constraints();
    c.pack_count = Some(4);
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let four = RetailerProduct {
        pack_count: Some(4),
        ..product("p4", Retailer::Tesco)
    };
    let six = RetailerProduct {
        pack_count: Some(6),
        ..product("p6", Retailer::Tesco)
    };

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m4", "ci-yog", "p4", MatchStatus::Accepted),
                four,
                offer("o4", "p4", 300, Some(5)),
            ),
            (
                product_match("m6", "ci-yog", "p6", MatchStatus::Suggested),
                six,
                offer("o6", "p6", 200, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("the 4-pack qualifies");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p4");
    assert_eq!(single.total_cents, 300, "cheaper 6-pack excluded");
}

#[test]
fn form_factor_filters_case_insensitively() {
    // form_factor = "can". The canned product (stored "Can") survives; the bottle
    // is rejected.
    let mut basket_item = item("beans", "ci-beans", 1);
    let mut c = empty_constraints();
    c.form_factor = Some("can".into());
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let can = RetailerProduct {
        form_factor: Some("Can".into()), // different case on the product side
        ..product("p-can", Retailer::Tesco)
    };
    let bottle = RetailerProduct {
        form_factor: Some("bottle".into()),
        ..product("p-bot", Retailer::Tesco)
    };

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-c", "ci-beans", "p-can", MatchStatus::Accepted),
                can,
                offer("o-c", "p-can", 150, Some(5)),
            ),
            (
                product_match("m-b", "ci-beans", "p-bot", MatchStatus::Suggested),
                bottle,
                offer("o-b", "p-bot", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("the can qualifies");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-can");
    assert_eq!(single.total_cents, 150, "cheaper bottle excluded");
}

#[test]
fn dietary_flags_require_product_superset() {
    // Required = [gluten_free, vegan]. A product carrying both (plus extras)
    // qualifies; one carrying only gluten_free is rejected.
    let mut basket_item = item("crackers", "ci-crk", 1);
    let mut c = empty_constraints();
    c.dietary_flags = Some(vec!["gluten_free".into(), "vegan".into()]);
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let superset = RetailerProduct {
        dietary_flags: Some(vec!["gluten_free".into(), "vegan".into(), "organic".into()]),
        ..product("p-ok", Retailer::Tesco)
    };
    let partial = RetailerProduct {
        dietary_flags: Some(vec!["gluten_free".into()]),
        ..product("p-no", Retailer::Tesco)
    };

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-ok", "ci-crk", "p-ok", MatchStatus::Accepted),
                superset,
                offer("o-ok", "p-ok", 300, Some(5)),
            ),
            (
                product_match("m-no", "ci-crk", "p-no", MatchStatus::Suggested),
                partial,
                offer("o-no", "p-no", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("the superset product qualifies");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-ok");
    assert_eq!(
        single.total_cents, 300,
        "the partial-flag product is excluded"
    );
}

#[test]
fn none_constraints_pass_all_candidates() {
    // No constraints row at all: every priced candidate survives the filter and
    // the cheapest is chosen (regression guard for the `constraints == None`
    // short-circuit in check_constraints).
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-a", "ci-milk", "p-a", MatchStatus::Accepted),
                brand_only_product("p-a", Retailer::Tesco, "Avonmore"),
                offer("o-a", "p-a", 200, Some(5)),
            ),
            (
                product_match("m-g", "ci-milk", "p-g", MatchStatus::Suggested),
                brand_only_product("p-g", Retailer::Tesco, "Generic"),
                offer("o-g", "p-g", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("an unconstrained item is priceable");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(
        single.total_cents, 100,
        "with no constraints the cheapest candidate wins"
    );
    assert!(out.no_valid_offer_items.is_empty());
}

#[test]
fn empty_allowed_brands_list_behaves_like_none() {
    // An empty allowed_brands list (vs None) must NOT reject every product — the
    // filter treats `Some([])` as "no allow-list", so all candidates survive.
    let mut basket_item = item("milk", "ci-milk", 1);
    let mut c = empty_constraints();
    c.allowed_brands = Some(vec![]);
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-g", "ci-milk", "p-g", MatchStatus::Accepted),
            brand_only_product("p-g", Retailer::Tesco, "Generic"),
            offer("o-g", "p-g", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let single = out
        .best_single
        .expect("empty allow-list does not reject everything");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.total_cents, 100);
}

#[test]
fn brandless_product_passes_excluded_brands() {
    // A product with no brand cannot be on the block-list, so excluded_brands
    // must let it through (it is not blocked by a name it does not have).
    let mut basket_item = item("apples", "ci-apple", 1);
    let mut c = empty_constraints();
    c.excluded_brands = Some(vec!["Generic".into()]);
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    // `product()` has brand: None.
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-x", "ci-apple", "p-x", MatchStatus::Accepted),
            product("p-x", Retailer::Tesco),
            offer("o-x", "p-x", 150, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let single = out
        .best_single
        .expect("a brand-less product is not on the block-list");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.total_cents, 150);
}

// ---------------------------------------------------------------------------
// Backlog-T1: substitution policy (Strict / Ask)
// ---------------------------------------------------------------------------

#[test]
fn strict_policy_shows_missing_when_exact_product_has_no_offer() {
    // substitution_policy = Strict. The exact (accepted) product has no offer
    // available; only a suggested substitute does. Strict must surface the item
    // as missing (StrictNoExactOffer) rather than offering the substitute.
    let mut basket_item = item("ketchup", "ci-k", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Strict;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    // Only a *suggested* substitute reaches the optimiser with an offer (the
    // exact match's product had no current offer, so db.rs never emitted a row
    // for it). Strict must not substitute it.
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-sub", "ci-k", "p-sub", MatchStatus::Suggested),
            product("p-sub", Retailer::Tesco),
            offer("o-sub", "p-sub", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    assert!(out.best_single.is_none(), "no plan: substitute not offered");
    assert!(out.unresolved_items.is_empty());
    assert!(out.needs_confirmation_items.is_empty());
    assert_eq!(out.no_valid_offer_items.len(), 1);
    assert_eq!(
        out.no_valid_offer_items[0].reason,
        NoOfferReason::StrictNoExactOffer
    );
}

#[test]
fn strict_policy_with_only_suggested_matches_is_strict_no_exact_offer() {
    // Pins the chosen "exact match" semantic: under Strict, an item whose ONLY
    // matches are `Suggested` (no `Accepted` one at all) — even with perfectly
    // good, constraint-passing offers — is reported StrictNoExactOffer rather
    // than priced. A suggested match is not an exact match.
    let mut basket_item = item("ketchup", "ci-k", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Strict;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-s1", "ci-k", "p-s1", MatchStatus::Suggested),
                product("p-s1", Retailer::Tesco),
                offer("o-s1", "p-s1", 120, Some(5)),
            ),
            (
                product_match("m-s2", "ci-k", "p-s2", MatchStatus::Suggested),
                product("p-s2", Retailer::Aldi),
                offer("o-s2", "p-s2", 110, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    assert!(
        out.best_single.is_none(),
        "Strict offers no plan when no accepted match exists"
    );
    assert_eq!(out.no_valid_offer_items.len(), 1);
    assert_eq!(
        out.no_valid_offer_items[0].reason,
        NoOfferReason::StrictNoExactOffer
    );
}

#[test]
fn strict_policy_prices_the_exact_accepted_match() {
    // substitution_policy = Strict and the exact (accepted) product DOES have an
    // offer: it is priced normally, while a suggested substitute is dropped.
    let mut basket_item = item("ketchup", "ci-k", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Strict;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-x", "ci-k", "p-exact", MatchStatus::Accepted),
                product("p-exact", Retailer::Tesco),
                offer("o-x", "p-exact", 250, Some(5)),
            ),
            (
                product_match("m-sub", "ci-k", "p-sub", MatchStatus::Suggested),
                product("p-sub", Retailer::Tesco),
                offer("o-sub", "p-sub", 100, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("exact match is priceable");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-exact");
    assert_eq!(single.total_cents, 250, "cheaper substitute dropped");
    assert!(out.no_valid_offer_items.is_empty());
}

#[test]
fn ask_policy_surfaces_item_for_confirmation() {
    // substitution_policy = Ask. A qualifying candidate exists, but the item is
    // held back for confirmation rather than auto-priced.
    let mut basket_item = item("cereal", "ci-c", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Ask;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-1", "ci-c", "p-1", MatchStatus::Accepted),
                product("p-1", Retailer::Tesco),
                offer("o-1", "p-1", 300, Some(5)),
            ),
            (
                product_match("m-2", "ci-c", "p-2", MatchStatus::Suggested),
                product("p-2", Retailer::Aldi),
                offer("o-2", "p-2", 250, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    assert!(
        out.best_single.is_none(),
        "Ask items are not auto-priced into a plan"
    );
    assert!(out.no_valid_offer_items.is_empty());
    assert_eq!(out.needs_confirmation_items.len(), 1);
    let nc = &out.needs_confirmation_items[0];
    assert_eq!(nc.item_index, 0);
    assert_eq!(nc.display_text, "cereal");
    assert_eq!(nc.canonical_item_id, "ci-c");
    assert_eq!(
        nc.candidate_count, 2,
        "both qualifying candidates are counted"
    );
    // Issue #78: the candidate offers are carried so the UI can render a chooser,
    // cheapest-effective-price first. p-2 (250) sorts before p-1 (300).
    assert_eq!(nc.candidates.len(), 2);
    assert_eq!(nc.candidate_count, nc.candidates.len());
    assert_eq!(nc.candidates[0].retailer_product_id, "p-2");
    assert_eq!(nc.candidates[0].price_cents, 250);
    assert_eq!(nc.candidates[0].retailer, Retailer::Aldi);
    assert_eq!(nc.candidates[1].retailer_product_id, "p-1");
    assert_eq!(nc.candidates[1].price_cents, 300);
}

#[test]
fn ask_policy_carries_product_details_on_each_candidate() {
    // The chooser needs name/brand/size/price/retailer per candidate (issue #78).
    let mut basket_item = item("dish soap", "ci-d", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Ask;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-1", "ci-d", "p-fairy", MatchStatus::Suggested),
            branded_product("p-fairy", Retailer::Tesco, "Fairy", 500.0, "ml"),
            // Shelf 300, loyalty 250 → effective 250 surfaces on the candidate.
            loyalty_offer("o-1", "p-fairy", 300, 250),
        )],
    );

    let out = run(&input, priced);
    let nc = &out.needs_confirmation_items[0];
    let cand = &nc.candidates[0];
    assert_eq!(cand.retailer_product_id, "p-fairy");
    assert_eq!(cand.name, "p-fairy");
    assert_eq!(cand.brand.as_deref(), Some("Fairy"));
    assert_eq!(cand.size_text.as_deref(), Some("500ml"));
    assert_eq!(
        cand.price_cents, 250,
        "effective (loyalty) price is carried"
    );
    assert_eq!(cand.retailer, Retailer::Tesco);
}

#[test]
fn ask_policy_confirmed_prefer_prices_chosen_candidate() {
    // Issue #78: once the household confirms a product (a persisted `prefer`,
    // here injected via the preferred map), the ask item enters the plan priced
    // at the CHOSEN candidate — not the globally cheapest one — and leaves the
    // confirmation list.
    let mut basket_item = item("cereal", "ci-c", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Ask;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-1", "ci-c", "p-1", MatchStatus::Accepted),
                product("p-1", Retailer::Tesco),
                offer("o-1", "p-1", 300, Some(5)),
            ),
            (
                product_match("m-2", "ci-c", "p-2", MatchStatus::Suggested),
                product("p-2", Retailer::Aldi),
                offer("o-2", "p-2", 250, Some(5)),
            ),
        ],
    );

    // Household chose the dearer p-1; the optimiser must honour that exact pick.
    let mut preferred = BTreeMap::new();
    preferred.insert(0, "p-1".to_string());

    let out = run_with_preferred(&input, priced, preferred);
    assert!(
        out.needs_confirmation_items.is_empty(),
        "a confirmed ask item is no longer awaiting confirmation"
    );
    let single = out
        .best_single
        .expect("confirmed item is priced into a plan");
    assert_eq!(single.lines.len(), 1);
    assert_eq!(single.lines[0].retailer_product_id, "p-1");
    assert_eq!(
        single.total_cents, 300,
        "priced at the chosen candidate, not the cheaper p-2"
    );
}

#[test]
fn ask_policy_stale_prefer_falls_back_to_confirmation() {
    // A persisted `prefer` whose product is no longer among the qualifying
    // candidates (e.g. its offer lapsed) must not crash or silently price; the
    // item falls back to awaiting a fresh choice (issue #78).
    let mut basket_item = item("cereal", "ci-c", 1);
    let mut c = empty_constraints();
    c.substitution_policy = SubstitutionPolicy::Ask;
    basket_item.constraints = Some(c);
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-1", "ci-c", "p-1", MatchStatus::Accepted),
            product("p-1", Retailer::Tesco),
            offer("o-1", "p-1", 300, Some(5)),
        )],
    );

    // Points at a product that isn't a current candidate.
    let mut preferred = BTreeMap::new();
    preferred.insert(0, "p-gone".to_string());

    let out = run_with_preferred(&input, priced, preferred);
    assert!(out.best_single.is_none(), "stale choice is not priced");
    assert_eq!(out.needs_confirmation_items.len(), 1);
    assert_eq!(out.needs_confirmation_items[0].candidates.len(), 1);
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
        entry_id: None,
        display_text: "yoghurt".into(),
        canonical_item_id: None,
        count: 1,
        constraint_policy: ConstraintPolicy::Any,
        constraints: None,
        category_id: None,
        subcategory_id: None,
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

// ---------------------------------------------------------------------------
// Household store settings (issue #98)
// ---------------------------------------------------------------------------

/// With no store settings and no trip, every retailer stays eligible — the
/// pre-#98 behaviour. (Regression guard for the new eligibility filter.)
#[test]
fn default_store_settings_leave_every_retailer_active() {
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-a", "ci-milk", "p-a", MatchStatus::Accepted),
                product("p-a", Retailer::Aldi),
                offer("o-a", "p-a", 100, Some(5)),
            ),
            (
                product_match("m-t", "ci-milk", "p-t", MatchStatus::Accepted),
                product("p-t", Retailer::Tesco),
                offer("o-t", "p-t", 150, Some(5)),
            ),
        ],
    );
    let input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    let out = run(&input, priced);
    let single = out.best_single.expect("plan");
    assert_eq!(single.stores, vec![Retailer::Aldi]);
    assert_eq!(single.total_cents, 100);
}

/// A store set to `off` is dropped from every plan and from the theoretical
/// benchmark, even though it is the cheapest.
#[test]
fn off_store_is_excluded_from_every_plan() {
    let mut input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    input.store_settings = vec![store_setting(Retailer::Aldi, StoreStatus::Off, false)];

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-a", "ci-milk", "p-a", MatchStatus::Accepted),
                product("p-a", Retailer::Aldi),
                offer("o-a", "p-a", 100, Some(5)),
            ),
            (
                product_match("m-t", "ci-milk", "p-t", MatchStatus::Accepted),
                product("p-t", Retailer::Tesco),
                offer("o-t", "p-t", 150, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.expect("plan");
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(single.total_cents, 150);
    let benchmark = out.theoretical_minimum.expect("benchmark");
    assert_eq!(benchmark.stores, vec![Retailer::Tesco]);
}

/// An `occasional` store is excluded until the household marks a trip to it.
#[test]
fn occasional_store_is_excluded_until_marked_as_a_trip() {
    let mut input = OptimiserInput::new("hh-1", Some(5), vec![item("hamper", "ci-hamper", 1)]);
    input.store_settings = vec![store_setting(
        Retailer::Dunnes,
        StoreStatus::Occasional,
        false,
    )];

    let out = run(&input, dunnes_rows(1000));
    assert!(out.best_single.is_none(), "occasional store not on a trip");
    assert!(out.best_two_store.is_none());
    assert!(out.theoretical_minimum.is_none());

    input.trip_retailers = vec![Retailer::Dunnes];
    let out = run(&input, dunnes_rows(1000));
    let single = out.best_single.expect("plan");
    assert_eq!(single.stores, vec![Retailer::Dunnes]);
    assert_eq!(single.total_cents, 1000);
}

/// Acceptance (issue #98): with Dunnes set to `occasional`, no plan includes it
/// and no voucher hint is shown unless the household marks a Dunnes trip.
#[test]
fn dunnes_voucher_hint_hidden_when_occasional_and_no_trip() {
    let mut input = OptimiserInput::new("hh-1", Some(5), vec![item("hamper", "ci-hamper", 1)]);
    input.store_settings = vec![store_setting(
        Retailer::Dunnes,
        StoreStatus::Occasional,
        false,
    )];

    // €48.00 would sit in the €45–€50 voucher window, but Dunnes is not eligible,
    // so no plan carries the hint.
    let out = run(&input, dunnes_rows(4800));
    assert!(out.best_single.is_none());
    assert!(out.theoretical_minimum.is_none());

    // Marking the trip admits Dunnes, and the hint appears.
    input.trip_retailers = vec![Retailer::Dunnes];
    let out = run(&input, dunnes_rows(4800));
    let single = out.best_single.expect("plan");
    assert_eq!(single.stores, vec![Retailer::Dunnes]);
    assert_eq!(
        single.dunnes_voucher_offer_text,
        Some("add €2.00 more to potentially trigger the €50 voucher".into())
    );
}

/// A trip only admits `occasional` stores; an `off` store stays out.
#[test]
fn trip_does_not_admit_an_off_store() {
    let mut input = OptimiserInput::new("hh-1", Some(5), vec![item("hamper", "ci-hamper", 1)]);
    input.store_settings = vec![store_setting(Retailer::Dunnes, StoreStatus::Off, false)];
    input.trip_retailers = vec![Retailer::Dunnes];

    let out = run(&input, dunnes_rows(1000));
    assert!(out.best_single.is_none());
    assert!(out.theoretical_minimum.is_none());
}

/// Acceptance (issue #98): a nearby store is recommended for a smaller saving
/// than a distant one. A €7.00 saving is below the €10.00 default second-store
/// bar but above the €5.00 nearby bar, so only the split touching a nearby store
/// is recommended.
#[test]
fn nearby_store_recommended_for_a_smaller_saving_than_a_distant_one() {
    let distant = recommendation_with_saving_and_settings(700, Vec::new());
    assert_eq!(distant.kind, RecommendedKind::SingleStore);
    assert!(!distant.clears_convenience_threshold);

    let nearby = recommendation_with_saving_and_settings(
        700,
        vec![store_setting(Retailer::Tesco, StoreStatus::Active, true)],
    );
    assert_eq!(nearby.kind, RecommendedKind::TwoStore);
    assert!(nearby.clears_convenience_threshold);
    assert_eq!(nearby.two_store_saving_cents, 700);
}

#[test]
fn nearby_existing_single_store_does_not_lower_bar_for_distant_added_store() {
    let input = two_store_split_input(
        DEFAULT_THRESHOLD_2_CENTS,
        vec![store_setting(Retailer::Aldi, StoreStatus::Active, true)],
    );
    let out = run(
        &input,
        two_item_retailer_rows(&[
            (Retailer::Aldi, 10_000, 30_000),
            (Retailer::Lidl, 10_600, 29_400),
        ]),
    );

    assert_eq!(out.best_single.unwrap().stores, vec![Retailer::Aldi]);
    let best_split = out.best_two_store.expect("two-store split");
    assert_eq!(best_split.total_cents, 39_400);
    assert_eq!(best_split.stores.len(), 2);
    let recommendation = out.recommended.expect("recommendation");
    assert_eq!(recommendation.kind, RecommendedKind::SingleStore);
    assert_eq!(recommendation.two_store_saving_cents, 600);
    assert_eq!(recommendation.plan.stores, vec![Retailer::Aldi]);
}

#[test]
fn qualifying_nearby_pair_is_considered_when_cheaper_distant_pair_fails() {
    let input = two_store_split_input(
        DEFAULT_THRESHOLD_2_CENTS,
        vec![store_setting(
            Retailer::SuperValu,
            StoreStatus::Active,
            true,
        )],
    );
    let out = run(
        &input,
        two_item_retailer_rows(&[
            (Retailer::Tesco, 20_000, 20_000),
            (Retailer::Lidl, 19_200, 30_000),
            (Retailer::SuperValu, 19_400, 30_000),
        ]),
    );

    assert_eq!(out.best_single.unwrap().stores, vec![Retailer::Tesco]);
    assert_eq!(out.best_two_store.unwrap().total_cents, 39_200);
    let recommendation = out.recommended.expect("recommendation");
    assert_eq!(recommendation.kind, RecommendedKind::TwoStore);
    assert_eq!(recommendation.two_store_saving_cents, 600);
    assert!(recommendation.plan.stores.contains(&Retailer::Tesco));
    assert!(recommendation.plan.stores.contains(&Retailer::SuperValu));
    assert!(!recommendation.plan.stores.contains(&Retailer::Lidl));
}

/// The nearby bar is a floor, not a replacement: a household that set a smaller
/// threshold than €5 keeps it even when the split touches a nearby store.
#[test]
fn nearby_threshold_never_raises_a_smaller_household_threshold() {
    // Household threshold €3.00 (300) < nearby €5.00 (500); a €4.00 (400) saving
    // clears €3, so the split is recommended.
    let input = two_store_split_input(
        300,
        vec![store_setting(Retailer::Tesco, StoreStatus::Active, true)],
    );
    let rec = run(&input, two_store_split_rows(400))
        .recommended
        .expect("recommendation");
    assert_eq!(rec.kind, RecommendedKind::TwoStore);
    assert!(rec.clears_convenience_threshold);
}

// ---------------------------------------------------------------------------
// Per-store category rules (issue #99)
// ---------------------------------------------------------------------------

const PRODUCE: &str = "cat-produce";
const PANTRY: &str = "cat-pantry";
const BREAKFAST: &str = "sub-breakfast";
const PASTA_RICE: &str = "sub-pasta-rice";

/// A resolved item in the given category (and optional sub-category).
fn item_in(
    display: &str,
    canonical_id: &str,
    category_id: &str,
    subcategory_id: Option<&str>,
) -> BasketItem {
    let mut it = item(display, canonical_id, 1);
    it.category_id = Some(category_id.into());
    it.subcategory_id = subcategory_id.map(Into::into);
    it
}

/// One category rule.
fn category_rule(category_id: &str, subcategory_id: Option<&str>, allowed: bool) -> CategoryRule {
    CategoryRule {
        category_id: category_id.into(),
        subcategory_id: subcategory_id.map(Into::into),
        allowed,
    }
}

/// An active Aldi store carrying the given category rules.
fn aldi_with_rules(rules: Vec<CategoryRule>) -> StoreSetting {
    let mut setting = store_setting(Retailer::Aldi, StoreStatus::Active, false);
    setting.category_rules = rules;
    setting
}

/// Acceptance (issue #99): with Aldi's Produce excluded, no plan ever places the
/// fresh item at Aldi, while the packaged item still can be when Aldi is cheapest.
#[test]
fn fresh_item_never_placed_at_a_store_that_excludes_its_category() {
    let mut input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![
            item_in("apple", "ci-apple", PRODUCE, None),
            item_in("pasta", "ci-pasta", PANTRY, None),
        ],
    );
    input.store_settings = vec![aldi_with_rules(vec![category_rule(PRODUCE, None, false)])];

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    // apple: Aldi 50 (cheapest), Tesco 200.
    priced.insert(
        0,
        vec![
            (
                product_match("m-aa", "ci-apple", "p-aa", MatchStatus::Accepted),
                product("p-aa", Retailer::Aldi),
                offer("o-aa", "p-aa", 50, Some(5)),
            ),
            (
                product_match("m-at", "ci-apple", "p-at", MatchStatus::Accepted),
                product("p-at", Retailer::Tesco),
                offer("o-at", "p-at", 200, Some(5)),
            ),
        ],
    );
    // pasta: Aldi 100 (cheapest), Tesco 200.
    priced.insert(
        1,
        vec![
            (
                product_match("m-pa", "ci-pasta", "p-pa", MatchStatus::Accepted),
                product("p-pa", Retailer::Aldi),
                offer("o-pa", "p-pa", 100, Some(5)),
            ),
            (
                product_match("m-pt", "ci-pasta", "p-pt", MatchStatus::Accepted),
                product("p-pt", Retailer::Tesco),
                offer("o-pt", "p-pt", 200, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);

    // The fresh item is never routed to Aldi in ANY published plan.
    for plan in [
        out.best_single.as_ref(),
        out.best_two_store.as_ref(),
        out.theoretical_minimum.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        assert!(
            !plan
                .lines
                .iter()
                .any(|l| l.item_index == 0 && l.retailer == Retailer::Aldi),
            "fresh item must never be placed at Aldi: {plan:?}"
        );
    }
    // The packaged item CAN be at Aldi when cheapest: the benchmark picks it there.
    let bench = out.theoretical_minimum.as_ref().expect("benchmark");
    assert!(
        bench
            .lines
            .iter()
            .any(|l| l.item_index == 1 && l.retailer == Retailer::Aldi),
        "packaged item should be priced at the cheapest store: {bench:?}"
    );
    // Completeness: Tesco is the only complete single store (Aldi misses the apple).
    assert_eq!(
        out.best_single.as_ref().expect("single").stores,
        vec![Retailer::Tesco]
    );
    // The recommendation explains why the cheaper store was skipped.
    let excluded = reason_with(&out, PlanKind::Recommended, ReasonCode::CategoryExcluded)
        .expect("category-excluded reason");
    assert_eq!(excluded.retailers, vec![Retailer::Aldi]);
    assert_eq!(excluded.item_indices, vec![0]);
}

/// A store with rules is still used for the categories it does allow, even when
/// it is the cheapest option overall.
#[test]
fn packaged_item_can_be_placed_at_a_store_with_category_rules() {
    let mut input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item_in("pasta", "ci-pasta", PANTRY, None)],
    );
    input.store_settings = vec![aldi_with_rules(vec![category_rule(PRODUCE, None, false)])];

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-pa", "ci-pasta", "p-pa", MatchStatus::Accepted),
                product("p-pa", Retailer::Aldi),
                offer("o-pa", "p-pa", 100, Some(5)),
            ),
            (
                product_match("m-pt", "ci-pasta", "p-pt", MatchStatus::Accepted),
                product("p-pt", Retailer::Tesco),
                offer("o-pt", "p-pt", 200, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let single = out.best_single.as_ref().expect("single");
    assert_eq!(single.stores, vec![Retailer::Aldi]);
    assert_eq!(single.total_cents, 100);
    assert!(
        reason_with(&out, PlanKind::Recommended, ReasonCode::CategoryExcluded).is_none(),
        "no rule excludes this category, so no exclusion reason"
    );
}

/// A sub-category rule refines its category rule: excluding Pantry but allowing
/// Breakfast lets the breakfast item stay at Aldi while the rest of Pantry moves.
#[test]
fn subcategory_allow_rule_overrides_a_category_exclusion() {
    let mut input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![
            item_in("cereal", "ci-cereal", PANTRY, Some(BREAKFAST)),
            item_in("pasta", "ci-pasta", PANTRY, Some(PASTA_RICE)),
        ],
    );
    input.store_settings = vec![aldi_with_rules(vec![
        category_rule(PANTRY, None, false),
        category_rule(PANTRY, Some(BREAKFAST), true),
    ])];

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    // cereal: Aldi 100 (cheapest), Tesco 300.
    priced.insert(
        0,
        vec![
            (
                product_match("m-ca", "ci-cereal", "p-ca", MatchStatus::Accepted),
                product("p-ca", Retailer::Aldi),
                offer("o-ca", "p-ca", 100, Some(5)),
            ),
            (
                product_match("m-ct", "ci-cereal", "p-ct", MatchStatus::Accepted),
                product("p-ct", Retailer::Tesco),
                offer("o-ct", "p-ct", 300, Some(5)),
            ),
        ],
    );
    // pasta: Aldi 100 (cheapest), Tesco 200.
    priced.insert(
        1,
        vec![
            (
                product_match("m-pa", "ci-pasta", "p-pa", MatchStatus::Accepted),
                product("p-pa", Retailer::Aldi),
                offer("o-pa", "p-pa", 100, Some(5)),
            ),
            (
                product_match("m-pt", "ci-pasta", "p-pt", MatchStatus::Accepted),
                product("p-pt", Retailer::Tesco),
                offer("o-pt", "p-pt", 200, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let bench = out.theoretical_minimum.as_ref().expect("benchmark");
    // Breakfast is re-allowed at Aldi; the rest of Pantry is not.
    assert!(bench
        .lines
        .iter()
        .any(|l| l.item_index == 0 && l.retailer == Retailer::Aldi));
    assert!(!bench
        .lines
        .iter()
        .any(|l| l.item_index == 1 && l.retailer == Retailer::Aldi));
}

/// A sub-category exclusion under an otherwise allowed category keeps the rest
/// of that category at the store.
#[test]
fn subcategory_exclusion_applies_within_an_allowed_category() {
    let mut input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![
            item_in("cereal", "ci-cereal", PANTRY, Some(BREAKFAST)),
            item_in("pasta", "ci-pasta", PANTRY, Some(PASTA_RICE)),
        ],
    );
    input.store_settings = vec![aldi_with_rules(vec![category_rule(
        PANTRY,
        Some(BREAKFAST),
        false,
    )])];

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-ca", "ci-cereal", "p-ca", MatchStatus::Accepted),
                product("p-ca", Retailer::Aldi),
                offer("o-ca", "p-ca", 100, Some(5)),
            ),
            (
                product_match("m-ct", "ci-cereal", "p-ct", MatchStatus::Accepted),
                product("p-ct", Retailer::Tesco),
                offer("o-ct", "p-ct", 300, Some(5)),
            ),
        ],
    );
    priced.insert(
        1,
        vec![
            (
                product_match("m-pa", "ci-pasta", "p-pa", MatchStatus::Accepted),
                product("p-pa", Retailer::Aldi),
                offer("o-pa", "p-pa", 100, Some(5)),
            ),
            (
                product_match("m-pt", "ci-pasta", "p-pt", MatchStatus::Accepted),
                product("p-pt", Retailer::Tesco),
                offer("o-pt", "p-pt", 200, Some(5)),
            ),
        ],
    );

    let out = run(&input, priced);
    let bench = out.theoretical_minimum.as_ref().expect("benchmark");
    assert!(!bench
        .lines
        .iter()
        .any(|l| l.item_index == 0 && l.retailer == Retailer::Aldi));
    assert!(bench
        .lines
        .iter()
        .any(|l| l.item_index == 1 && l.retailer == Retailer::Aldi));
}

/// An item we cannot classify (no category) is never excluded by a category
/// rule — we drop unpriceable unknowns, never silently lose a resolved item.
#[test]
fn unclassified_item_is_not_excluded_by_category_rules() {
    let mut input = OptimiserInput::new("hh-1", Some(5), vec![item("mystery", "ci-x", 1)]);
    input.store_settings = vec![aldi_with_rules(vec![category_rule(PRODUCE, None, false)])];

    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-x", "p", MatchStatus::Accepted),
            product("p", Retailer::Aldi),
            offer("o", "p", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    assert_eq!(
        out.best_single.expect("single").stores,
        vec![Retailer::Aldi]
    );
}

// ---------------------------------------------------------------------------
// Plan reasons (issue #103)
// ---------------------------------------------------------------------------

/// An offer carrying an explicit source snapshot date.
fn offer_dated(
    id: &str,
    product_id: &str,
    price_cents: Cents,
    at_fetch: Option<i64>,
    snapshot_date: Option<&str>,
) -> ProductOffer {
    ProductOffer {
        source_snapshot_date: snapshot_date.map(Into::into),
        ..offer(id, product_id, price_cents, at_fetch)
    }
}

/// The first reason with `code` for `kind`, if any.
fn reason_with(out: &OptimiserOutput, kind: PlanKind, code: ReasonCode) -> Option<&PlanReason> {
    out.reasons
        .iter()
        .find(|r| r.plan == kind && r.code == code)
}

/// Basket used by the reason tests: milk cheapest at Aldi, bread cheapest at
/// Tesco, so the two-store split genuinely spans both stores.
fn two_by_two_input() -> OptimiserInput {
    OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("bread", "ci-bread", 1)],
    )
}

/// Priced rows for [`two_by_two_input`]: milk Aldi 100 / Tesco 150, bread Tesco
/// 180 / Aldi 220. Best single = Aldi 320; split = 280 (saving 40).
fn two_by_two_rows() -> BTreeMap<usize, Rows> {
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-ma", "ci-milk", "p-ma", MatchStatus::Accepted),
                product("p-ma", Retailer::Aldi),
                offer("o-ma", "p-ma", 100, Some(5)),
            ),
            (
                product_match("m-mt", "ci-milk", "p-mt", MatchStatus::Accepted),
                product("p-mt", Retailer::Tesco),
                offer("o-mt", "p-mt", 150, Some(5)),
            ),
        ],
    );
    priced.insert(
        1,
        vec![
            (
                product_match("m-bt", "ci-bread", "p-bt", MatchStatus::Accepted),
                product("p-bt", Retailer::Tesco),
                offer("o-bt", "p-bt", 180, Some(5)),
            ),
            (
                product_match("m-ba", "ci-bread", "p-ba", MatchStatus::Accepted),
                product("p-ba", Retailer::Aldi),
                offer("o-ba", "p-ba", 220, Some(5)),
            ),
        ],
    );
    priced
}

#[test]
fn reason_credits_the_cheapest_store_and_reports_coverage() {
    let out = run(&two_by_two_input(), two_by_two_rows());

    // Aldi alone is cheapest and complete (320 vs Tesco 330); the split saves only
    // €0.40, below the €10 bar, so Aldi is recommended.
    let single = out.best_single.as_ref().expect("single");
    assert_eq!(single.stores, vec![Retailer::Aldi]);
    assert_eq!(single.total_cents, 320);

    // Aldi is credited as cheapest for milk only — bread is cheapest at Tesco.
    let cheapest = reason_with(&out, PlanKind::BestSingle, ReasonCode::CheapestAtStore)
        .expect("cheapest-at-store reason");
    assert_eq!(cheapest.retailers, vec![Retailer::Aldi]);
    assert_eq!(cheapest.item_indices, vec![0]);

    // Coffee-table: every published plan states its coverage.
    for kind in [
        PlanKind::Recommended,
        PlanKind::BestSingle,
        PlanKind::BestTwoStore,
        PlanKind::TheoreticalMinimum,
    ] {
        let coverage = reason_with(&out, kind, ReasonCode::Coverage)
            .unwrap_or_else(|| panic!("{kind:?} has no coverage reason"));
        assert!(
            !coverage.item_indices.is_empty(),
            "{kind:?} coverage is empty"
        );
    }
    let coverage = reason_with(&out, PlanKind::BestSingle, ReasonCode::Coverage).expect("coverage");
    assert_eq!(coverage.item_indices, vec![0, 1]);
    assert_eq!(coverage.cents, Some(320));

    // The rejected Tesco alternative is reported like-for-like (same coverage).
    let rejected =
        reason_with(&out, PlanKind::BestSingle, ReasonCode::RejectedStore).expect("rejected store");
    assert_eq!(rejected.retailers, vec![Retailer::Tesco]);
    assert_eq!(rejected.cents, Some(320 - 330));

    // The recommendation explains the rejected split and its sub-bar saving.
    let rec = out.recommended.as_ref().expect("recommended");
    assert_eq!(rec.kind, RecommendedKind::SingleStore);
    let not_cleared = reason_with(&out, PlanKind::Recommended, ReasonCode::ThresholdNotCleared)
        .expect("threshold-not-cleared reason");
    assert_eq!(not_cleared.retailers, vec![Retailer::Tesco]);
    assert_eq!(not_cleared.cents, Some(40));
    assert_eq!(not_cleared.threshold_cents, Some(DEFAULT_THRESHOLD_2_CENTS));
}

#[test]
fn reason_reports_no_known_price_here_without_claiming_unavailability() {
    // milk 90 at Aldi only; bread 100 at Tesco only. Aldi is cheapest overall but
    // incomplete, so it wins on the fulfilment rule (both miss one item, Aldi is
    // cheaper).
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("bread", "ci-bread", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-ma", "ci-milk", "p-ma", MatchStatus::Accepted),
            product("p-ma", Retailer::Aldi),
            offer("o-ma", "p-ma", 90, Some(5)),
        )],
    );
    priced.insert(
        1,
        vec![(
            product_match("m-bt", "ci-bread", "p-bt", MatchStatus::Accepted),
            product("p-bt", Retailer::Tesco),
            offer("o-bt", "p-bt", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let single = out.best_single.as_ref().expect("single");
    assert_eq!(single.stores, vec![Retailer::Aldi]);
    assert_eq!(single.missing_count, 1);

    // Bread has a known price elsewhere but none at Aldi: "no known price here".
    let here = reason_with(&out, PlanKind::BestSingle, ReasonCode::NoKnownPriceHere)
        .expect("no-known-price-here reason");
    assert_eq!(here.item_indices, vec![1]);
    assert_eq!(here.retailers, vec![Retailer::Aldi]);

    // Milk is only priced at Aldi, so it gets the stronger only-store reason and
    // is deliberately NOT double-counted as merely "cheapest".
    let only = reason_with(&out, PlanKind::BestSingle, ReasonCode::OnlyStoreStocking)
        .expect("only-store-stocking reason");
    assert_eq!(only.item_indices, vec![0]);
    assert_eq!(only.retailers, vec![Retailer::Aldi]);
    assert!(reason_with(&out, PlanKind::BestSingle, ReasonCode::CheapestAtStore).is_none());
}

#[test]
fn reason_distinguishes_unpriced_and_unresolved_items() {
    // item0 is priced at Tesco; item1 resolves but has no matches anywhere; item2
    // was never resolved.
    let unresolved_item = BasketItem {
        entry_id: None,
        display_text: "cream".into(),
        canonical_item_id: None,
        count: 1,
        constraint_policy: ConstraintPolicy::Any,
        constraints: None,
        category_id: None,
        subcategory_id: None,
    };
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![
            item("milk", "ci-milk", 1),
            item("eggs", "ci-eggs", 1),
            unresolved_item,
        ],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-mt", "ci-milk", "p-mt", MatchStatus::Accepted),
            product("p-mt", Retailer::Tesco),
            offer("o-mt", "p-mt", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let anywhere = reason_with(&out, PlanKind::BestSingle, ReasonCode::NoKnownPriceAnywhere)
        .expect("no-known-price-anywhere reason");
    assert_eq!(anywhere.item_indices, vec![1]);
    let unresolved = reason_with(&out, PlanKind::BestSingle, ReasonCode::UnresolvedItems)
        .expect("unresolved reason");
    assert_eq!(unresolved.item_indices, vec![2]);
    // Only the priced line counts as coverage.
    let coverage = reason_with(&out, PlanKind::BestSingle, ReasonCode::Coverage).expect("coverage");
    assert_eq!(coverage.item_indices, vec![0]);
}

#[test]
fn reason_flags_coverage_differs_instead_of_a_false_saving() {
    // Tesco stocks both items and is complete; Aldi stocks only milk (cheap).
    // Aldi's cheap total covers less, so it must not be reported as a saving.
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("eggs", "ci-eggs", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![
            (
                product_match("m-ma", "ci-milk", "p-ma", MatchStatus::Accepted),
                product("p-ma", Retailer::Aldi),
                offer("o-ma", "p-ma", 50, Some(5)),
            ),
            (
                product_match("m-mt", "ci-milk", "p-mt", MatchStatus::Accepted),
                product("p-mt", Retailer::Tesco),
                offer("o-mt", "p-mt", 200, Some(5)),
            ),
        ],
    );
    priced.insert(
        1,
        vec![(
            product_match("m-et", "ci-eggs", "p-et", MatchStatus::Accepted),
            product("p-et", Retailer::Tesco),
            offer("o-et", "p-et", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let single = out.best_single.as_ref().expect("single");
    // Completeness beats cost: Tesco prices both items, Aldi only one.
    assert_eq!(single.stores, vec![Retailer::Tesco]);
    assert_eq!(single.missing_count, 0);

    assert!(
        reason_with(&out, PlanKind::BestSingle, ReasonCode::RejectedStore).is_none(),
        "a differing-coverage store must not be reported as a like-for-like alternative"
    );
    let differs = reason_with(&out, PlanKind::BestSingle, ReasonCode::CoverageDiffers)
        .expect("coverage-differs reason");
    assert_eq!(differs.retailers, vec![Retailer::Aldi]);
}

#[test]
fn equal_missing_counts_with_different_items_are_not_comparable() {
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("eggs", "ci-eggs", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-a", "ci-milk", "p-a", MatchStatus::Accepted),
            product("p-a", Retailer::Aldi),
            offer("o-a", "p-a", 50, Some(5)),
        )],
    );
    priced.insert(
        1,
        vec![(
            product_match("m-t", "ci-eggs", "p-t", MatchStatus::Accepted),
            product("p-t", Retailer::Tesco),
            offer("o-t", "p-t", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let alternative = reason_with(&out, PlanKind::BestSingle, ReasonCode::CoverageDiffers)
        .expect("different priced item set is not comparable");
    assert_eq!(alternative.retailers, vec![Retailer::Tesco]);
    assert_eq!(alternative.cents, None);
    assert!(reason_with(&out, PlanKind::BestSingle, ReasonCode::RejectedStore).is_none());
}

#[test]
fn reason_reports_the_cleared_threshold_for_a_split() {
    let out = run(
        &two_store_split_input(DEFAULT_THRESHOLD_2_CENTS, Vec::new()),
        two_store_split_rows(1001),
    );
    let rec = out.recommended.as_ref().expect("recommended");
    assert_eq!(rec.kind, RecommendedKind::TwoStore);
    let cleared = reason_with(&out, PlanKind::Recommended, ReasonCode::ThresholdCleared)
        .expect("threshold-cleared reason");
    assert_eq!(cleared.cents, Some(1001));
    assert_eq!(cleared.threshold_cents, Some(DEFAULT_THRESHOLD_2_CENTS));
    // The reason names the added store, not the single-store baseline (Aldi is
    // the cheaper single store; Tesco is the store the split adds).
    assert_eq!(cleared.retailers, vec![Retailer::Tesco]);
}

#[test]
fn reason_reports_the_lower_nearby_bar_when_applied() {
    let out = run(
        &two_store_split_input(
            DEFAULT_THRESHOLD_2_CENTS,
            vec![store_setting(Retailer::Tesco, StoreStatus::Active, true)],
        ),
        two_store_split_rows(700),
    );
    let rec = out.recommended.as_ref().expect("recommended");
    assert_eq!(rec.kind, RecommendedKind::TwoStore);
    let cleared = reason_with(&out, PlanKind::Recommended, ReasonCode::ThresholdCleared)
        .expect("threshold-cleared reason");
    // €7 clears the €5 nearby bar (the split adds nearby Tesco) even though it is
    // below the €10 default.
    assert_eq!(cleared.threshold_cents, Some(NEARBY_THRESHOLD_2_CENTS));
    assert_eq!(cleared.retailers, vec![Retailer::Tesco]);
}

#[test]
fn reason_marks_items_under_the_brand_rule() {
    let mut basket_item = item("dish soap", "ci-soap", 1);
    basket_item.constraint_policy = ConstraintPolicy::Brand;
    basket_item.constraints = Some(brand_constraints(Some("Fairy")));
    let input = OptimiserInput::new("hh-1", Some(5), vec![basket_item]);
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m", "ci-soap", "p", MatchStatus::Accepted),
            branded_product("p", Retailer::Tesco, "Fairy", 500.0, "ml"),
            offer("o", "p", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let brand =
        reason_with(&out, PlanKind::BestSingle, ReasonCode::BrandRule).expect("brand-rule reason");
    assert_eq!(brand.item_indices, vec![0]);
}

#[test]
fn reason_explains_stores_left_out_by_settings() {
    let mut input = OptimiserInput::new("hh-1", Some(5), vec![item("milk", "ci-milk", 1)]);
    input.store_settings = vec![
        store_setting(Retailer::Aldi, StoreStatus::Off, false),
        store_setting(Retailer::Dunnes, StoreStatus::Occasional, false),
    ];
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m-mt", "ci-milk", "p-mt", MatchStatus::Accepted),
            product("p-mt", Retailer::Tesco),
            offer("o-mt", "p-mt", 100, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let off =
        reason_with(&out, PlanKind::Recommended, ReasonCode::StoreOff).expect("store-off reason");
    assert_eq!(off.retailers, vec![Retailer::Aldi]);
    let not_on_trip = reason_with(&out, PlanKind::Recommended, ReasonCode::StoreNotOnTrip)
        .expect("store-not-on-trip reason");
    assert_eq!(not_on_trip.retailers, vec![Retailer::Dunnes]);
    // The store actually planned for is never listed as excluded.
    assert!(out
        .reasons
        .iter()
        .filter(|r| matches!(r.code, ReasonCode::StoreOff | ReasonCode::StoreNotOnTrip))
        .all(|r| !r.retailers.contains(&Retailer::Tesco)));
}

#[test]
fn reason_reports_price_date_and_stale_lines() {
    // Two Tesco lines: one fresh (run 5), one a run behind (Low confidence).
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("bread", "ci-bread", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m1", "ci-milk", "p1", MatchStatus::Accepted),
            product("p1", Retailer::Tesco),
            offer_dated("o1", "p1", 100, Some(5), Some("2026-06-01")),
        )],
    );
    priced.insert(
        1,
        vec![(
            product_match("m2", "ci-bread", "p2", MatchStatus::Accepted),
            product("p2", Retailer::Tesco),
            offer_dated("o2", "p2", 200, Some(4), Some("2026-05-20")),
        )],
    );

    let out = run(&input, priced);
    let as_of =
        reason_with(&out, PlanKind::BestSingle, ReasonCode::PriceAsOf).expect("price-as-of reason");
    assert_eq!(as_of.snapshot_date.as_deref(), Some("2026-05-20"));
    let stale = reason_with(&out, PlanKind::BestSingle, ReasonCode::StalePrices)
        .expect("stale-prices reason");
    assert_eq!(stale.item_indices, vec![1]);
    assert_eq!(stale.retailers, vec![Retailer::Tesco]);
}

#[test]
fn price_as_of_marks_lines_without_a_snapshot_date() {
    let input = OptimiserInput::new(
        "hh-1",
        Some(5),
        vec![item("milk", "ci-milk", 1), item("bread", "ci-bread", 1)],
    );
    let mut priced: BTreeMap<usize, Rows> = BTreeMap::new();
    priced.insert(
        0,
        vec![(
            product_match("m1", "ci-milk", "p1", MatchStatus::Accepted),
            product("p1", Retailer::Tesco),
            offer_dated("o1", "p1", 100, Some(5), Some("2026-06-01")),
        )],
    );
    priced.insert(
        1,
        vec![(
            product_match("m2", "ci-bread", "p2", MatchStatus::Accepted),
            product("p2", Retailer::Tesco),
            offer("o2", "p2", 200, Some(5)),
        )],
    );

    let out = run(&input, priced);
    let as_of =
        reason_with(&out, PlanKind::BestSingle, ReasonCode::PriceAsOf).expect("price-as-of reason");
    assert_eq!(as_of.snapshot_date.as_deref(), Some("2026-06-01"));
    assert_eq!(as_of.item_indices, vec![1]);
}

#[test]
fn every_published_plan_carries_coverage_and_freshness_reasons() {
    let out = run(&two_by_two_input(), two_by_two_rows());
    for kind in [
        PlanKind::Recommended,
        PlanKind::BestSingle,
        PlanKind::BestTwoStore,
        PlanKind::TheoreticalMinimum,
    ] {
        assert!(reason_with(&out, kind, ReasonCode::Coverage).is_some());
        assert!(reason_with(&out, kind, ReasonCode::PriceAsOf).is_some());
    }
}

#[test]
fn reasons_stay_consistent_with_the_plan_lines_they_describe() {
    let out = run(&two_by_two_input(), two_by_two_rows());
    for reason in &out.reasons {
        let plan = match reason.plan {
            PlanKind::Recommended => out.recommended.as_ref().map(|r| &r.plan),
            PlanKind::BestSingle => out.best_single.as_ref(),
            PlanKind::BestTwoStore => out.best_two_store.as_ref(),
            PlanKind::TheoreticalMinimum => out.theoretical_minimum.as_ref(),
        }
        .expect("a reason must reference a plan that exists");
        let line_items: std::collections::BTreeSet<usize> =
            plan.lines.iter().map(|l| l.item_index).collect();
        match reason.code {
            ReasonCode::CheapestAtStore | ReasonCode::OnlyStoreStocking | ReasonCode::Coverage => {
                assert!(
                    reason.item_indices.iter().all(|i| line_items.contains(i)),
                    "reason names items outside its plan: {reason:?}"
                );
            }
            ReasonCode::NoKnownPriceHere => {
                assert!(
                    reason.item_indices.iter().all(|i| !line_items.contains(i)),
                    "a missing-price reason must not name priced lines: {reason:?}"
                );
            }
            _ => {}
        }
        // Every reason either names something or is a plan-wide statement.
        assert!(
            !(reason.item_indices.is_empty()
                && reason.retailers.is_empty()
                && reason.cents.is_none()
                && reason.snapshot_date.is_none())
                || reason.code == ReasonCode::PriceAsOf,
            "reason carries no facts: {reason:?}"
        );
    }
}
