//! Canonical item → retailer product candidate ranking (issue #108).
//!
//! This is the second half of the matching problem, deliberately kept separate
//! from [`crate::matching`]:
//!
//! * [`crate::matching`] resolves **raw list text → canonical item** (alias
//!   lookup, token/fuzzy match, LLM fallback).
//! * This module proposes **canonical item → retailer product** candidates
//!   within the existing catalogue so a household can accept one and get a price.
//!
//! The two must not be conflated (the issue calls this out explicitly): a
//! canonical item is a household's abstract staple ("Baked beans"); a retailer
//! product is a concrete pack sold at one store ("Heinz Baked Beans 415g, Tesco").
//! Resolving text to a staple does not imply any product is a match.
//!
//! ## Deterministic ranking
//!
//! [`rank_candidates`] scores every candidate product against an [`ItemProfile`]
//! using normalised text similarity (exact → substring → Jaro-Winkler → token
//! overlap), plus a brand bonus/penalty and a preferred-size bonus. It is pure
//! and deterministic: the same profile + product list always yields the same
//! order, tie-broken by retailer then product name then id. Products whose brand
//! the household explicitly excludes, and products with no textual relation to
//! the item at all, are dropped entirely.
//!
//! The API layer optionally reorders the top candidates with the LLM fallback
//! (`crates/api/src/preferences.rs`), but the deterministic order is always the
//! baseline and the fallback can never introduce a product that this module did
//! not surface.

use std::collections::BTreeSet;

use strsim::jaro_winkler;

use crate::domain::{RetailerProduct, SizeBasis};
use crate::normalise::normalize;

/// How many candidates to keep per retailer. Bounds the proposal list so a store
/// with hundreds of loosely-related products cannot flood the UI; the rest are
/// still reachable by tightening the item's rules.
pub const MAX_CANDIDATES_PER_RETAILER: usize = 5;

/// Minimum name-similarity score for a product to be considered at all. Below
/// this the product shares no meaningful text with the item and is noise.
const MIN_NAME_SCORE: i32 = 40;

/// Weighted bonus for a product whose brand matches the item's brand rule.
const BRAND_MATCH_BONUS: i32 = 30;
/// Weighted bonus for a product whose brand is in the item's allowed set.
const ALLOWED_BRAND_BONUS: i32 = 22;
/// Penalty when the item names a brand (or non-empty allowed set) the product
/// does not satisfy. Non-fatal: the product stays a candidate, ranked lower.
const BRAND_MISMATCH_PENALTY: i32 = 20;
/// Bonus for a product whose size is within ±20% of the item's preferred size.
const SIZE_MATCH_BONUS: i32 = 18;

/// The household's view of a canonical item, reduced to the fields candidate
/// ranking needs. Built by the caller from `canonical_items` + `item_aliases` +
/// `item_constraints`.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemProfile {
    /// `canonical_items.name_en`.
    pub name_en: String,
    /// `canonical_items.name_es`.
    pub name_es: String,
    /// Every `item_aliases.alias` for the item (already normalised or raw; both
    /// are normalised again here).
    pub aliases: Vec<String>,
    /// `item_constraints.brand` (the `brand` policy's required brand).
    pub brand: Option<String>,
    /// `item_constraints.allowed_brands`.
    pub allowed_brands: Vec<String>,
    /// `item_constraints.excluded_brands` — a hard exclusion.
    pub excluded_brands: Vec<String>,
    /// `item_constraints.preferred_size` as a `(value, basis)` pair.
    pub preferred_size: Option<(f64, SizeBasis)>,
}

/// Why a product was scored the way it was. Rendered by the API as stable signal
/// codes so the UI can show the household what the proposal is based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchSignal {
    /// The product name is textually similar to the item's name/alias.
    Name,
    /// The product brand satisfies the item's brand rule.
    Brand,
    /// The product size is within the preferred band.
    Size,
}

impl MatchSignal {
    /// Stable wire code (`"name"` / `"brand"` / `"size"`).
    pub fn as_str(self) -> &'static str {
        match self {
            MatchSignal::Name => "name",
            MatchSignal::Brand => "brand",
            MatchSignal::Size => "size",
        }
    }
}

/// A scored candidate product. `score` orders candidates; `signals` explains the
/// ranking (name is always present, brand/size only when they contributed).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredProduct {
    pub product: RetailerProduct,
    pub score: i32,
    pub signals: Vec<MatchSignal>,
}

/// Rank retailer products as candidates for one canonical item.
///
/// Returns at most [`MAX_CANDIDATES_PER_RETAILER`] products per retailer, ordered
/// by retailer then descending score then product name then id — a fully
/// deterministic order. Products whose brand is excluded, and products with no
/// textual relation to the item, are omitted.
pub fn rank_candidates(profile: &ItemProfile, products: &[RetailerProduct]) -> Vec<ScoredProduct> {
    let mut scored: Vec<ScoredProduct> = products
        .iter()
        .filter_map(|p| score_product(profile, p))
        .collect();

    // Stable, deterministic ordering: retailer, then best score, then name, then id.
    scored.sort_by(|a, b| {
        a.product
            .retailer
            .as_db_str()
            .cmp(b.product.retailer.as_db_str())
            .then(b.score.cmp(&a.score))
            .then(a.product.name.cmp(&b.product.name))
            .then(a.product.id.cmp(&b.product.id))
    });

    // Keep the top N per retailer (the sorted order groups each retailer's
    // products together in descending score).
    let mut per_retailer: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    scored.retain(|c| {
        let count = per_retailer
            .entry(c.product.retailer.as_db_str())
            .or_insert(0);
        if *count < MAX_CANDIDATES_PER_RETAILER {
            *count += 1;
            true
        } else {
            false
        }
    });
    scored
}

/// Score one product, or `None` when it is excluded or unrelated.
fn score_product(profile: &ItemProfile, product: &RetailerProduct) -> Option<ScoredProduct> {
    let product_brand = product.brand.as_deref().map(normalize);

    // Hard exclusion: an explicitly excluded brand is never proposed, whatever
    // its name similarity.
    if let Some(pb) = product_brand.as_deref() {
        if profile.excluded_brands.iter().any(|b| normalize(b) == pb) {
            return None;
        }
    }

    let name_score = name_score(profile, &product.name);
    if name_score < MIN_NAME_SCORE {
        return None;
    }

    let mut score = name_score;
    let mut signals = vec![MatchSignal::Name];

    // Brand: a strong positive when the item's required brand matches, a milder
    // positive for an allowed-set member, and a penalty when the item names a
    // brand the product clearly is not.
    if let Some(pb) = product_brand.as_deref() {
        let required_match = profile.brand.as_deref().is_some_and(|b| normalize(b) == pb);
        let allowed_match = profile.allowed_brands.iter().any(|b| normalize(b) == pb);
        if required_match {
            score += BRAND_MATCH_BONUS;
            signals.push(MatchSignal::Brand);
        } else if allowed_match {
            score += ALLOWED_BRAND_BONUS;
            signals.push(MatchSignal::Brand);
        } else if profile
            .brand
            .as_deref()
            .is_some_and(|b| !normalize(b).is_empty())
            || !profile.allowed_brands.is_empty()
        {
            score -= BRAND_MISMATCH_PENALTY;
        }
    }

    if let Some((value, basis)) = profile.preferred_size {
        if size_within_band(product, value, basis) {
            score += SIZE_MATCH_BONUS;
            signals.push(MatchSignal::Size);
        }
    }

    Some(ScoredProduct {
        product: product.clone(),
        score,
        signals,
    })
}

/// Best textual similarity between the product name and any of the item's names
/// or aliases, on the 0..=100 scale described in [`string_similarity`].
fn name_score(profile: &ItemProfile, product_name: &str) -> i32 {
    let product_norm = normalize(product_name);
    let mut best = 0;
    let mut consider = |candidate: &str| {
        let candidate_norm = normalize(candidate);
        if candidate_norm.is_empty() {
            return;
        }
        let s = string_similarity(&candidate_norm, &product_norm);
        if s > best {
            best = s;
        }
    };
    consider(&profile.name_en);
    consider(&profile.name_es);
    for alias in &profile.aliases {
        consider(alias);
    }
    best
}

/// Textual similarity on a 0..=100 scale:
///
/// * `100` — exact normalised equality.
/// * `70` — one string contains the other (contained string ≥ 4 chars).
/// * `jw * 70` — Jaro-Winkler ≥ 0.80 (rounded), which tolerates small typos and
///   word-order/shape differences.
/// * `overlap * 50` — token Jaccard overlap, for multi-word names that share
///   meaningful tokens without one containing the other.
/// * `0` — unrelated.
fn string_similarity(a: &str, b: &str) -> i32 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a == b {
        return 100;
    }
    let (shorter, longer) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    if shorter.chars().count() >= 4 && longer.contains(shorter) {
        return 70;
    }
    let jw = jaro_winkler(a, b);
    if jw >= 0.80 {
        return (jw * 70.0).round() as i32;
    }
    let overlap = token_overlap(a, b);
    if overlap > 0.0 {
        return (overlap * 50.0).round() as i32;
    }
    0
}

/// Jaccard overlap of the two strings' tokens (whitespace-separated, length ≥ 2).
fn token_overlap(a: &str, b: &str) -> f64 {
    let ta: BTreeSet<&str> = a.split_whitespace().filter(|t| t.len() >= 2).collect();
    let tb: BTreeSet<&str> = b.split_whitespace().filter(|t| t.len() >= 2).collect();
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    intersection / union
}

/// Whether the product's normalised size is within ±20% of `value` on `basis`.
/// A product with no size, or a size on another basis, does not match.
fn size_within_band(product: &RetailerProduct, value: f64, basis: SizeBasis) -> bool {
    let Some(size_value) = product.size_value else {
        return false;
    };
    let Some(unit) = product.size_unit.as_deref() else {
        return false;
    };
    if SizeBasis::from_db_str(unit).ok() != Some(basis) {
        return false;
    }
    if value <= 0.0 {
        return false;
    }
    (size_value - value).abs() / value <= 0.20
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Retailer, Source};

    fn product(id: &str, retailer: Retailer, name: &str) -> RetailerProduct {
        RetailerProduct {
            id: id.into(),
            source: Source::Manual,
            retailer,
            external_id: None,
            url: None,
            name: name.into(),
            brand: None,
            category: None,
            pack_size_text: None,
            size_value: None,
            size_unit: None,
            pack_count: None,
            form_factor: None,
            dietary_flags: None,
            first_seen_at: "t".into(),
            last_seen_at: "t".into(),
        }
    }

    fn profile(name: &str) -> ItemProfile {
        ItemProfile {
            name_en: name.into(),
            name_es: name.into(),
            aliases: Vec::new(),
            brand: None,
            allowed_brands: Vec::new(),
            excluded_brands: Vec::new(),
            preferred_size: None,
        }
    }

    #[test]
    fn substring_name_match_beats_unrelated_products() {
        let p = profile("Baked beans");
        let products = vec![
            product("a", Retailer::Tesco, "Heinz Baked Beans 415g"),
            product("b", Retailer::Tesco, "Chicken Kiev 300g"),
        ];
        let ranked = rank_candidates(&p, &products);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].product.id, "a");
        assert_eq!(ranked[0].signals, vec![MatchSignal::Name]);
    }

    #[test]
    fn exact_name_outranks_partial() {
        let p = profile("Milk");
        let products = vec![
            product("partial", Retailer::Aldi, "Whole Milk 2L"),
            product("exact", Retailer::Aldi, "Milk"),
        ];
        let ranked = rank_candidates(&p, &products);
        assert_eq!(ranked[0].product.id, "exact");
    }

    #[test]
    fn excluded_brand_is_never_proposed() {
        let mut p = profile("Baked beans");
        p.excluded_brands = vec!["Heinz".into()];
        let mut heinz = product("heinz", Retailer::Tesco, "Heinz Baked Beans 415g");
        heinz.brand = Some("Heinz".into());
        let products = vec![
            heinz,
            product("other", Retailer::Tesco, "Own Baked Beans 400g"),
        ];
        let ranked = rank_candidates(&p, &products);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].product.id, "other");
    }

    #[test]
    fn matching_brand_outranks_unknown_brand() {
        let mut p = profile("Baked beans");
        p.brand = Some("Heinz".into());
        let mut heinz = product("heinz", Retailer::Tesco, "Heinz Baked Beans 415g");
        heinz.brand = Some("Heinz".into());
        let mut own = product("own", Retailer::Tesco, "Baked Beans Value 420g");
        own.brand = Some("Tesco".into());
        let ranked = rank_candidates(&p, &[own, heinz]);
        assert_eq!(ranked[0].product.id, "heinz");
        assert!(ranked[0].signals.contains(&MatchSignal::Brand));
        // The mismatched brand is still offered, just ranked lower.
        assert_eq!(ranked[1].product.id, "own");
    }

    #[test]
    fn preferred_size_adds_a_signal_and_tiebreaks() {
        let mut p = profile("Baked beans");
        p.preferred_size = Some((415.0, SizeBasis::Grams));
        let mut right = product("right", Retailer::Tesco, "Heinz Baked Beans");
        right.size_value = Some(415.0);
        right.size_unit = Some("g".into());
        let mut wrong = product("wrong", Retailer::Tesco, "Heinz Baked Beans");
        wrong.size_value = Some(200.0);
        wrong.size_unit = Some("g".into());
        let ranked = rank_candidates(&p, &[wrong, right]);
        assert_eq!(ranked[0].product.id, "right");
        assert!(ranked[0].signals.contains(&MatchSignal::Size));
    }

    #[test]
    fn limits_candidates_per_retailer_and_is_deterministic() {
        let p = profile("Milk");
        let products: Vec<_> = (0..8)
            .map(|i| product(&format!("t{i}"), Retailer::Tesco, &format!("Milk {i}")))
            .chain((0..3).map(|i| product(&format!("a{i}"), Retailer::Aldi, &format!("Milk {i}"))))
            .collect();
        let ranked = rank_candidates(&p, &products);
        let tesco = ranked
            .iter()
            .filter(|c| c.product.retailer == Retailer::Tesco)
            .count();
        let aldi = ranked
            .iter()
            .filter(|c| c.product.retailer == Retailer::Aldi)
            .count();
        assert_eq!(tesco, MAX_CANDIDATES_PER_RETAILER);
        assert_eq!(aldi, 3);
        // Deterministic across runs.
        assert_eq!(rank_candidates(&p, &products), ranked);
    }

    #[test]
    fn unrelated_products_are_dropped() {
        let p = profile("Bananas");
        let products = vec![product("x", Retailer::Tesco, "Laundry Detergent 2L")];
        assert!(rank_candidates(&p, &products).is_empty());
    }
}
