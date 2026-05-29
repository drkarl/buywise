//! Size and unit-price normalisation.
//!
//! The optimiser compares *unit* prices (€/L, €/kg, €/each, …), not shelf
//! prices. This module parses retailer pack-size text into a base unit
//! (weight → grams, volume → millilitres, discrete → each) and computes the
//! unit price in micros (€ × 1_000_000).

use crate::domain::{Micros, UnitPriceBasis};

/// A pack size normalised to a base unit: weight in grams, volume in
/// millilitres, or a discrete count.
///
/// `PartialEq` compares the `f64` fields bit-for-bit. It is reliable for exact
/// conversions (e.g. `"2L"` → `2000.0`) but **do not** `assert_eq!` on
/// `Weight`/`Volume` produced by non-exact conversions such as `lb`/`oz`/`pt`
/// — compare the field within an epsilon instead (see the imperial tests).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NormalisedSize {
    Weight { grams: f64 },
    Volume { ml: f64 },
    Count { each: u32 },
}

/// Divergence tolerance for `validate_unit_price`. A BasketWatch-provided unit
/// price within this fraction of the computed value is trusted as-is.
const DIVERGENCE_TOLERANCE: f64 = 0.02;

/// Parse retailer pack-size text into a [`NormalisedSize`].
///
/// Handles weight (`"500g"`, `"1.5kg"`, `"1lb"`), volume (`"2L"`, `"750ml"`,
/// `"1pt"`), and discrete counts (`"12 pack"`, `"6 rolls"`, `"40 sheets"`).
/// Matching is case-insensitive and tolerant of an optional space between the
/// number and the unit. Returns `None` for unparseable input.
///
/// ```
/// use buywise_core::normalise::{parse_pack_size, NormalisedSize};
/// assert_eq!(parse_pack_size("2L"), Some(NormalisedSize::Volume { ml: 2000.0 }));
/// assert_eq!(parse_pack_size("500g"), Some(NormalisedSize::Weight { grams: 500.0 }));
/// assert_eq!(parse_pack_size("12 pack"), Some(NormalisedSize::Count { each: 12 }));
/// ```
pub fn parse_pack_size(text: &str) -> Option<NormalisedSize> {
    let lower = text.trim().to_lowercase();
    let (number, unit) = split_number_unit(&lower)?;

    // Weight units → grams.
    match unit {
        "kg" => {
            return Some(NormalisedSize::Weight {
                grams: number * 1000.0,
            })
        }
        "g" | "gr" | "grams" | "gram" => {
            return Some(NormalisedSize::Weight { grams: number });
        }
        "lb" | "lbs" => {
            return Some(NormalisedSize::Weight {
                grams: number * 453.592,
            })
        }
        "oz" => {
            return Some(NormalisedSize::Weight {
                grams: number * 28.3495,
            })
        }
        // Volume units → millilitres.
        "l" | "ltr" | "litre" | "litres" | "liter" | "liters" => {
            return Some(NormalisedSize::Volume {
                ml: number * 1000.0,
            });
        }
        "cl" => return Some(NormalisedSize::Volume { ml: number * 10.0 }),
        "ml" => return Some(NormalisedSize::Volume { ml: number }),
        "pt" | "pint" | "pints" => {
            return Some(NormalisedSize::Volume {
                ml: number * 568.261,
            })
        }
        _ => {}
    }

    // Discrete count: keywords (or no unit at all) → each.
    if is_count_unit(unit) {
        // Counts are whole numbers; reject fractional pack counts. (`number`
        // cannot be negative here: `split_number_unit` only consumes ASCII
        // digits and '.', so a leading '-' is never part of the number.)
        if number.fract() != 0.0 {
            return None;
        }
        return Some(NormalisedSize::Count {
            each: number as u32,
        });
    }

    None
}

/// Split a lowercased, trimmed pack-size string into its leading numeric value
/// and trailing unit token. The space between number and unit is optional.
/// Returns `None` if no leading number is present.
fn split_number_unit(s: &str) -> Option<(f64, &str)> {
    let split_at = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    if split_at == 0 {
        return None; // No leading number.
    }
    let number: f64 = s[..split_at].parse().ok()?;
    let unit = s[split_at..].trim();
    Some((number, unit))
}

/// Whether a trailing token denotes a discrete count (or is empty, meaning a
/// bare number like "12"). Recognises common pluralised pack words.
fn is_count_unit(unit: &str) -> bool {
    matches!(
        unit,
        "" | "pack"
            | "packs"
            | "x"
            | "ct"
            | "count"
            | "each"
            | "pcs"
            | "pc"
            | "piece"
            | "pieces"
            | "roll"
            | "rolls"
            | "sheet"
            | "sheets"
            | "nappy"
            | "nappies"
            | "pod"
            | "pods"
            | "capsule"
            | "capsules"
            | "wash"
            | "washes"
            | "tablet"
            | "tablets"
            | "bag"
            | "bags"
            // "tin"/"tins" are intentionally counts: tinned goods (e.g. "4 tins
            // of beans") are compared per-tin, and the actual content size is
            // carried separately by the pack-size text. By contrast "bottle"/
            // "can" denote volume containers and are *not* counts here — a bottle
            // is parsed via its volume text (e.g. "750ml") so €/L stays correct.
            | "tin"
            | "tins"
            | "unit"
            | "units"
    )
}

/// Compute the unit price in micros (€ × 1_000_000) for a shelf price given in
/// cents and a normalised pack size.
///
/// `PerL`/`PerKg` divide by the base quantity expressed in litres/kilograms
/// (millilitres or grams ÷ 1000). All count-based bases (`PerEach`,
/// `PerNappy`, `PerPod`, `PerWash`, `PerSheet`, `PerRoll`) divide by the item
/// count. Returns `None` if `price_cents` is zero/negative, the size is
/// zero/negative, or the size and basis are dimensionally incompatible (e.g.
/// `PerL` with a `Count` size).
///
/// The count path is pure `i128` integer arithmetic (no floating-point money
/// error); the `PerL`/`PerKg` weight/volume paths divide by a fractional base
/// (e.g. 1.5kg → 1500.0) using an `f64` step then round to the nearest micro.
/// That step is exact for realistic prices (`cents × 1e10` stays integer-exact
/// in `f64` below ~€9k). The worked example €2.99 / 2L yields exactly
/// `1_495_000`.
pub fn compute_unit_price_micros(
    price_cents: i64,
    size: &NormalisedSize,
    basis: UnitPriceBasis,
) -> Option<Micros> {
    // A non-positive price is meaningless for a unit-price comparison.
    if price_cents <= 0 {
        return None;
    }
    // micros = cents × 10_000  (cents → € is ÷100, € → micros is ×1_000_000).
    // We fold the per-base division into one integer expression.
    match (basis, size) {
        // Volume → €/L: divide cents by litres (ml / 1000).
        (UnitPriceBasis::PerL, NormalisedSize::Volume { ml }) => {
            div_micros_by_base(price_cents, *ml)
        }
        // Weight → €/kg: divide cents by kilograms (g / 1000).
        (UnitPriceBasis::PerKg, NormalisedSize::Weight { grams }) => {
            div_micros_by_base(price_cents, *grams)
        }
        // Count-based bases divide by the item count.
        (
            UnitPriceBasis::PerEach
            | UnitPriceBasis::PerNappy
            | UnitPriceBasis::PerPod
            | UnitPriceBasis::PerWash
            | UnitPriceBasis::PerSheet
            | UnitPriceBasis::PerRoll,
            NormalisedSize::Count { each },
        ) => {
            if *each == 0 {
                return None;
            }
            // cents × 10_000 / count, rounded to nearest micro.
            let micros_total = (price_cents as i128) * 10_000;
            Some(div_round_nearest(micros_total, *each as i128) as i64)
        }
        // Any other size/basis pairing is dimensionally incompatible.
        _ => None,
    }
}

/// Helper for the 1000-unit bases (`PerL`, `PerKg`): given a price in cents and
/// a base quantity in millilitres or grams, return micros per litre/kilogram.
///
/// micros_per_base_unit = cents × 10_000 / (base / 1000)
///                      = cents × 10_000 × 1000 / base
fn div_micros_by_base(price_cents: i64, base: f64) -> Option<Micros> {
    if base <= 0.0 || !base.is_finite() {
        return None;
    }
    // base (ml or g) is a measured dimensional value and may be fractional
    // (e.g. 1.5kg → 1500.0). Use f64 for the division then round to nearest
    // micro; integer money invariants are preserved on the result (i64 micros).
    let numerator = (price_cents as f64) * 10_000.0 * 1000.0;
    let micros = (numerator / base).round();
    Some(micros as i64)
}

/// Integer division rounding to the nearest integer (half away from zero).
fn div_round_nearest(numerator: i128, denominator: i128) -> i128 {
    let half = denominator / 2;
    if (numerator >= 0) == (denominator >= 0) {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
    }
}

/// Map a product's display text to a [`UnitPriceBasis`] using category
/// keywords (see design §9). First match wins; defaults to `PerEach`.
///
/// Matching is by whole word, not substring, so "dishwasher" does not match
/// the "wash" keyword and "washing-up liquid" is not mis-read as `PerWash`.
/// More-specific categories (pod/capsule, liquid) are checked before the
/// broader "wash" category so e.g. "dishwasher pods" resolves to `PerPod`.
///
/// Retained as the design §9 category classifier for the phase-1 path that will
/// infer a basis from a product *name* (not pack-size text); `validate_unit_price`
/// no longer calls it (see its doc comment). `#[allow(dead_code)]` until that
/// caller is wired up.
#[allow(dead_code)]
fn infer_basis_from_text(text: &str) -> UnitPriceBasis {
    let t = text.to_lowercase();
    // Whole-word membership test: split on any non-alphanumeric character so
    // "washing-up" yields the tokens ["washing", "up"], not a "wash" match.
    let has_word = |kw: &str| t.split(|c: char| !c.is_alphanumeric()).any(|w| w == kw);

    // Toilet/kitchen roll → €/sheet (€/roll is the fallback handled elsewhere).
    if t.contains("toilet roll") || t.contains("kitchen roll") {
        return UnitPriceBasis::PerSheet;
    }
    // "napp" covers "nappy"/"nappies"; check the token prefix explicitly.
    if t.split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with("napp"))
    {
        return UnitPriceBasis::PerNappy;
    }
    // Liquids → €/L. Checked before "wash" so "washing-up liquid" → PerL.
    if has_word("milk") || has_word("juice") || has_word("liquid") {
        return UnitPriceBasis::PerL;
    }
    // Pods/capsules/tablets → €/pod (dosed units). Checked before "wash" so
    // "dishwasher pods"/"dishwasher tablets" → PerPod, not PerWash.
    if has_word("pod")
        || has_word("pods")
        || has_word("capsule")
        || has_word("capsules")
        || has_word("tablet")
        || has_word("tablets")
    {
        return UnitPriceBasis::PerPod;
    }
    if has_word("laundry") || has_word("detergent") || has_word("wash") || has_word("washes") {
        return UnitPriceBasis::PerWash;
    }
    if has_word("rice") || has_word("pasta") || has_word("flour") || has_word("dry") {
        return UnitPriceBasis::PerKg;
    }
    UnitPriceBasis::PerEach
}

/// Validate a BasketWatch-provided unit price and recompute it when necessary.
///
/// BasketWatch supplies unit prices; this cross-checks them against a value
/// computed from the pack-size text. If the provided value is within
/// [`DIVERGENCE_TOLERANCE`] (2%) of the computed value, the provided value is
/// trusted. If it diverges by more than 2%, the computed value is returned
/// instead.
///
/// The basis is taken from `provided_basis`. It is **not** inferred from
/// `pack_size_text`, which carries a size (e.g. "2L", "500g") rather than a
/// product category, so when `provided_basis` is `None` the returned basis is
/// also `None` (and no recompute is attempted).
///
/// "Warning" semantics: a >2% divergence is surfaced by *recomputing* — the
/// returned value differs from the provided one. If a pack size is unavailable
/// we cannot recompute, so the provided value is passed through.
// TODO(phase-1): add a structured divergence signal (e.g. a tracing event or a
// returned diagnostic) when a consumer needs to observe corrections, rather
// than only inferring it from a changed return value. Deferred here to avoid a
// speculative public-API change.
pub fn validate_unit_price(
    price_cents: i64,
    pack_size_text: Option<&str>,
    provided_micros: Option<Micros>,
    provided_basis: Option<UnitPriceBasis>,
) -> (Option<Micros>, Option<UnitPriceBasis>) {
    // The basis is whatever the caller provided; pack-size text is a size, not
    // a category, so it cannot supply a basis.
    let basis = provided_basis;

    // Attempt to compute a reference value from the pack size.
    let computed = match (pack_size_text, basis) {
        (Some(text), Some(b)) => {
            parse_pack_size(text).and_then(|size| compute_unit_price_micros(price_cents, &size, b))
        }
        _ => None,
    };

    match (provided_micros, computed) {
        // We can cross-check: trust provided if within tolerance, else recompute.
        (Some(provided), Some(computed)) => {
            if within_tolerance(provided, computed, DIVERGENCE_TOLERANCE) {
                (Some(provided), basis)
            } else {
                (Some(computed), basis)
            }
        }
        // No provided value: supplement with the computed value.
        (None, Some(computed)) => (Some(computed), basis),
        // Cannot recompute (no/unparseable pack size): pass the provided value through.
        (Some(provided), None) => (Some(provided), basis),
        // Nothing to work with.
        (None, None) => (None, basis),
    }
}

/// Whether `provided` is within `tolerance` (a fraction, e.g. 0.02 for 2%) of
/// `reference`. Divergence is measured relative to the reference value.
fn within_tolerance(provided: i64, reference: i64, tolerance: f64) -> bool {
    if reference == 0 {
        return provided == 0;
    }
    let diff = (provided - reference).abs() as f64;
    diff / (reference.abs() as f64) <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_pack_size: acceptance criteria ----

    #[test]
    fn parse_volume_litres() {
        assert_eq!(
            parse_pack_size("2L"),
            Some(NormalisedSize::Volume { ml: 2000.0 })
        );
    }

    #[test]
    fn parse_weight_grams() {
        assert_eq!(
            parse_pack_size("500g"),
            Some(NormalisedSize::Weight { grams: 500.0 })
        );
    }

    #[test]
    fn parse_count_pack() {
        assert_eq!(
            parse_pack_size("12 pack"),
            Some(NormalisedSize::Count { each: 12 })
        );
    }

    // ---- parse_pack_size: format coverage ----

    #[test]
    fn parse_is_case_insensitive_and_space_tolerant() {
        assert_eq!(
            parse_pack_size("2 l"),
            Some(NormalisedSize::Volume { ml: 2000.0 })
        );
        assert_eq!(
            parse_pack_size("2L"),
            Some(NormalisedSize::Volume { ml: 2000.0 })
        );
        assert_eq!(
            parse_pack_size("500 g"),
            Some(NormalisedSize::Weight { grams: 500.0 })
        );
        assert_eq!(
            parse_pack_size("500G"),
            Some(NormalisedSize::Weight { grams: 500.0 })
        );
    }

    #[test]
    fn parse_kilograms_and_millilitres() {
        assert_eq!(
            parse_pack_size("1.5kg"),
            Some(NormalisedSize::Weight { grams: 1500.0 })
        );
        assert_eq!(
            parse_pack_size("750ml"),
            Some(NormalisedSize::Volume { ml: 750.0 })
        );
        assert_eq!(
            parse_pack_size("1 kg"),
            Some(NormalisedSize::Weight { grams: 1000.0 })
        );
    }

    #[test]
    fn parse_centilitres() {
        assert_eq!(
            parse_pack_size("33cl"),
            Some(NormalisedSize::Volume { ml: 330.0 })
        );
    }

    #[test]
    fn parse_count_variants() {
        assert_eq!(
            parse_pack_size("12pack"),
            Some(NormalisedSize::Count { each: 12 })
        );
        assert_eq!(
            parse_pack_size("6 rolls"),
            Some(NormalisedSize::Count { each: 6 })
        );
        assert_eq!(
            parse_pack_size("40 sheets"),
            Some(NormalisedSize::Count { each: 40 })
        );
        assert_eq!(
            parse_pack_size("12 nappies"),
            Some(NormalisedSize::Count { each: 12 })
        );
        // Bare number is treated as a count.
        assert_eq!(
            parse_pack_size("8"),
            Some(NormalisedSize::Count { each: 8 })
        );
    }

    #[test]
    fn parse_imperial_weight_and_volume() {
        // 1lb ≈ 453.592 g, 1pt ≈ 568.261 ml.
        match parse_pack_size("1lb").unwrap() {
            NormalisedSize::Weight { grams } => assert!((grams - 453.592).abs() < 0.001),
            other => panic!("expected weight, got {other:?}"),
        }
        match parse_pack_size("1pt").unwrap() {
            NormalisedSize::Volume { ml } => assert!((ml - 568.261).abs() < 0.001),
            other => panic!("expected volume, got {other:?}"),
        }
        // 8oz ≈ 226.796 g.
        match parse_pack_size("8oz").unwrap() {
            NormalisedSize::Weight { grams } => assert!((grams - 8.0 * 28.3495).abs() < 0.001),
            other => panic!("expected weight, got {other:?}"),
        }
    }

    #[test]
    fn parse_bottle_and_can_are_not_counts() {
        // "bottle"/"can" denote volume containers, not counts; without a numeric
        // volume unit they are unparseable (the volume text carries the size).
        assert_eq!(parse_pack_size("1 bottle"), None);
        assert_eq!(parse_pack_size("6 cans"), None);
        // "tin"/"tins" remain intentional counts.
        assert_eq!(
            parse_pack_size("4 tins"),
            Some(NormalisedSize::Count { each: 4 })
        );
    }

    #[test]
    fn parse_rejects_unparseable() {
        assert_eq!(parse_pack_size(""), None);
        assert_eq!(parse_pack_size("large"), None);
        assert_eq!(parse_pack_size("L2"), None); // unit-first is not supported
        assert_eq!(parse_pack_size("2 furlongs"), None);
        // Fractional counts are invalid.
        assert_eq!(parse_pack_size("1.5 pack"), None);
    }

    // ---- compute_unit_price_micros: known pairs ----

    #[test]
    fn compute_volume_per_litre_acceptance() {
        // €2.99 / 2L = €1.495/L = 1_495_000 micros.
        let size = NormalisedSize::Volume { ml: 2000.0 };
        assert_eq!(
            compute_unit_price_micros(299, &size, UnitPriceBasis::PerL),
            Some(1_495_000)
        );
    }

    #[test]
    fn compute_weight_per_kg() {
        // €1.00 / 500g = €2.00/kg = 2_000_000 micros.
        let size = NormalisedSize::Weight { grams: 500.0 };
        assert_eq!(
            compute_unit_price_micros(100, &size, UnitPriceBasis::PerKg),
            Some(2_000_000)
        );
    }

    #[test]
    fn compute_weight_per_kg_one_kg() {
        // €3.50 / 1kg = €3.50/kg = 3_500_000 micros.
        let size = NormalisedSize::Weight { grams: 1000.0 };
        assert_eq!(
            compute_unit_price_micros(350, &size, UnitPriceBasis::PerKg),
            Some(3_500_000)
        );
    }

    #[test]
    fn compute_count_per_each() {
        // €2.40 / 12 = €0.20/each = 200_000 micros.
        let size = NormalisedSize::Count { each: 12 };
        assert_eq!(
            compute_unit_price_micros(240, &size, UnitPriceBasis::PerEach),
            Some(200_000)
        );
    }

    #[test]
    fn compute_count_per_nappy_and_sheet() {
        let size = NormalisedSize::Count { each: 40 };
        // €4.00 / 40 sheets = €0.10/sheet = 100_000 micros.
        assert_eq!(
            compute_unit_price_micros(400, &size, UnitPriceBasis::PerSheet),
            Some(100_000)
        );
        // Same count works for PerNappy / PerRoll / PerPod / PerWash.
        assert_eq!(
            compute_unit_price_micros(400, &size, UnitPriceBasis::PerNappy),
            Some(100_000)
        );
        assert_eq!(
            compute_unit_price_micros(400, &size, UnitPriceBasis::PerRoll),
            Some(100_000)
        );
    }

    #[test]
    fn compute_rounds_to_nearest_micro() {
        // €1.00 / 3 each = €0.3333…/each → 333_333 micros (rounded).
        let size = NormalisedSize::Count { each: 3 };
        assert_eq!(
            compute_unit_price_micros(100, &size, UnitPriceBasis::PerEach),
            Some(333_333)
        );
    }

    #[test]
    fn compute_rejects_incompatible_and_zero() {
        // PerL with a Count size is dimensionally incompatible.
        let count = NormalisedSize::Count { each: 6 };
        assert_eq!(
            compute_unit_price_micros(299, &count, UnitPriceBasis::PerL),
            None
        );
        // PerEach with a Volume size is incompatible.
        let vol = NormalisedSize::Volume { ml: 1000.0 };
        assert_eq!(
            compute_unit_price_micros(299, &vol, UnitPriceBasis::PerEach),
            None
        );
        // Zero size yields None.
        let zero_vol = NormalisedSize::Volume { ml: 0.0 };
        assert_eq!(
            compute_unit_price_micros(299, &zero_vol, UnitPriceBasis::PerL),
            None
        );
        let zero_count = NormalisedSize::Count { each: 0 };
        assert_eq!(
            compute_unit_price_micros(299, &zero_count, UnitPriceBasis::PerEach),
            None
        );
    }

    #[test]
    fn compute_rejects_zero_and_negative_price() {
        // A non-positive shelf price is meaningless for a unit-price comparison.
        let vol = NormalisedSize::Volume { ml: 2000.0 };
        assert_eq!(
            compute_unit_price_micros(0, &vol, UnitPriceBasis::PerL),
            None
        );
        assert_eq!(
            compute_unit_price_micros(-100, &vol, UnitPriceBasis::PerL),
            None
        );
        let count = NormalisedSize::Count { each: 12 };
        assert_eq!(
            compute_unit_price_micros(-100, &count, UnitPriceBasis::PerEach),
            None
        );
        // And validate_unit_price propagates None sanely: no computed reference,
        // so the provided value passes through.
        let (micros, basis) = validate_unit_price(
            -100,
            Some("2L"),
            Some(1_500_000),
            Some(UnitPriceBasis::PerL),
        );
        assert_eq!(micros, Some(1_500_000));
        assert_eq!(basis, Some(UnitPriceBasis::PerL));
    }

    // ---- validate_unit_price: 2% tolerance edges ----

    #[test]
    fn validate_trusts_provided_within_tolerance() {
        // Computed for "2L" PerL = 1_495_000. Provided 1_500_000 → ~0.33% diff (< 2%).
        let (micros, basis) =
            validate_unit_price(299, Some("2L"), Some(1_500_000), Some(UnitPriceBasis::PerL));
        assert_eq!(micros, Some(1_500_000));
        assert_eq!(basis, Some(UnitPriceBasis::PerL));
    }

    #[test]
    fn validate_recomputes_when_diverging() {
        // Provided 2_000_000 vs computed 1_495_000 → ~33% diff (> 2%) → recompute.
        let (micros, basis) =
            validate_unit_price(299, Some("2L"), Some(2_000_000), Some(UnitPriceBasis::PerL));
        assert_eq!(micros, Some(1_495_000));
        assert_eq!(basis, Some(UnitPriceBasis::PerL));
    }

    // Reference for the tolerance edge tests: €2.00 / 2kg PerKg = €1.00/kg = 1_000_000 micros.
    #[test]
    fn validate_tolerance_just_under_2pct() {
        let computed = compute_unit_price_micros(
            200,
            &NormalisedSize::Weight { grams: 2000.0 },
            UnitPriceBasis::PerKg,
        )
        .unwrap();
        assert_eq!(computed, 1_000_000);
        let provided = 1_019_900; // +1.99%
        let (micros, _) = validate_unit_price(
            200,
            Some("2kg"),
            Some(provided),
            Some(UnitPriceBasis::PerKg),
        );
        assert_eq!(micros, Some(provided), "just under 2% must be trusted");
    }

    #[test]
    fn validate_tolerance_exactly_2pct_is_trusted() {
        // Exactly 2% is within tolerance (<=).
        let provided = 1_020_000; // +2.00% of 1_000_000
        let (micros, _) = validate_unit_price(
            200,
            Some("2kg"),
            Some(provided),
            Some(UnitPriceBasis::PerKg),
        );
        assert_eq!(micros, Some(provided), "exactly 2% is trusted");
    }

    #[test]
    fn validate_tolerance_just_over_2pct_recomputes() {
        // 2.01% deviation → 1_020_100, outside tolerance → recompute to 1_000_000.
        let provided = 1_020_100; // +2.01%
        let (micros, _) = validate_unit_price(
            200,
            Some("2kg"),
            Some(provided),
            Some(UnitPriceBasis::PerKg),
        );
        assert_eq!(micros, Some(1_000_000), "just over 2% must recompute");
    }

    #[test]
    fn validate_supplements_when_no_provided_value() {
        // No provided unit price → compute and supplement.
        let (micros, basis) =
            validate_unit_price(299, Some("2L"), None, Some(UnitPriceBasis::PerL));
        assert_eq!(micros, Some(1_495_000));
        assert_eq!(basis, Some(UnitPriceBasis::PerL));
    }

    #[test]
    fn validate_passes_provided_through_when_uncomputable() {
        // No pack size to recompute from → trust the provided value.
        let (micros, basis) =
            validate_unit_price(299, None, Some(1_234_000), Some(UnitPriceBasis::PerL));
        assert_eq!(micros, Some(1_234_000));
        assert_eq!(basis, Some(UnitPriceBasis::PerL));
    }

    #[test]
    fn validate_does_not_infer_basis_from_pack_size_text() {
        // Realistic input: a pack size ("2L") with no provided basis. Pack-size
        // text is a size, not a category, so the basis stays None and no
        // recompute is attempted (we have nothing to divide by). The provided
        // unit price (if any) is passed through unchanged.
        let (micros, basis) = validate_unit_price(299, Some("2L"), None, None);
        assert_eq!(basis, None);
        assert_eq!(micros, None);

        let (micros, basis) = validate_unit_price(299, Some("500g"), Some(1_234_000), None);
        assert_eq!(basis, None);
        assert_eq!(micros, Some(1_234_000), "provided value passes through");
    }

    #[test]
    fn validate_returns_none_when_nothing_available() {
        let (micros, basis) = validate_unit_price(299, None, None, None);
        assert_eq!(micros, None);
        assert_eq!(basis, None);
    }

    // ---- infer_basis_from_text: category mapping ----

    #[test]
    fn infer_basis_keyword_mapping() {
        assert_eq!(infer_basis_from_text("Fresh milk"), UnitPriceBasis::PerL);
        assert_eq!(infer_basis_from_text("orange juice"), UnitPriceBasis::PerL);
        assert_eq!(infer_basis_from_text("basmati rice"), UnitPriceBasis::PerKg);
        assert_eq!(infer_basis_from_text("penne pasta"), UnitPriceBasis::PerKg);
        assert_eq!(infer_basis_from_text("plain flour"), UnitPriceBasis::PerKg);
        assert_eq!(
            infer_basis_from_text("size 4 nappies"),
            UnitPriceBasis::PerNappy
        );
        assert_eq!(
            infer_basis_from_text("laundry detergent"),
            UnitPriceBasis::PerWash
        );
        assert_eq!(infer_basis_from_text("coffee pods"), UnitPriceBasis::PerPod);
        assert_eq!(
            infer_basis_from_text("coffee capsules"),
            UnitPriceBasis::PerPod
        );
        assert_eq!(
            infer_basis_from_text("toilet roll 9 pack"),
            UnitPriceBasis::PerSheet
        );
        assert_eq!(
            infer_basis_from_text("kitchen roll"),
            UnitPriceBasis::PerSheet
        );
        assert_eq!(
            infer_basis_from_text("tin of beans"),
            UnitPriceBasis::PerEach
        );
    }

    #[test]
    fn infer_basis_avoids_substring_collisions() {
        // "dishwasher" must not match the "wash" keyword; pods win.
        assert_eq!(
            infer_basis_from_text("dishwasher pods"),
            UnitPriceBasis::PerPod
        );
        assert_eq!(
            infer_basis_from_text("dishwasher tablets"),
            UnitPriceBasis::PerPod
        );
        // "washing-up liquid" is a liquid (€/L), not a wash.
        assert_eq!(
            infer_basis_from_text("washing-up liquid"),
            UnitPriceBasis::PerL
        );
        // A genuine "wash" word still maps to PerWash.
        assert_eq!(
            infer_basis_from_text("non-bio wash detergent"),
            UnitPriceBasis::PerWash
        );
    }
}
