//! Best-effort extraction of `form_factor` and `dietary_flags` from product text
//! (Backlog-T1).
//!
//! Neither the BasketWatch RapidAPI feed nor the Lidl.ie feed carries structured
//! form-factor or dietary-attribute fields, but `retailer_products` now has the
//! two columns and the optimiser enforces them. Rather than silently writing
//! `NULL` (the plan forbids dropping the fields), both adapters route product
//! name + category through these scanners so the columns are populated whenever
//! the text makes the attribute unambiguous, and `None` otherwise.
//!
//! The vocabularies are the canonical ones declared in
//! [`buywise_core::domain::FORM_FACTORS`] / [`buywise_core::domain::DIETARY_FLAGS`];
//! these scanners only ever emit tokens from those sets so constraints and
//! products agree on spelling.

use buywise_core::domain::{DIETARY_FLAGS, FORM_FACTORS};

/// Derive a best-effort `(form_factor, dietary_flags)` pair from a product's
/// `name` and optional `category` text.
///
/// Matching is whole-word (token-based) and case-insensitive throughout, for
/// both attribute types. A form factor is emitted only when exactly one distinct
/// canonical form keyword is present; an ambiguous name carrying two or more
/// (e.g. both "can" and "tin") yields `None` rather than an arbitrary pick.
/// Dietary flags accumulate every canonical flag whose marker phrase appears as
/// a whole-word token sequence (so "organic" does not match "inorganic").
/// Absent signals yield `None` (an empty dietary set is represented as `None`,
/// not `Some(vec![])`, so it satisfies only an empty constraint).
pub fn extract_attributes(
    name: &str,
    category: Option<&str>,
) -> (Option<String>, Option<Vec<String>>) {
    let mut haystack = name.to_lowercase();
    if let Some(c) = category {
        haystack.push(' ');
        haystack.push_str(&c.to_lowercase());
    }
    let words: Vec<&str> = haystack
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    (form_factor(&words), dietary_flags(&words))
}

/// The single canonical [`FORM_FACTORS`] token appearing as a whole word, or
/// `None`. Returns `None` when none match **and** when more than one distinct
/// form keyword matches: an ambiguous product (e.g. "Tin Can ...") cannot be
/// classified into one physical form, so we decline rather than guess.
fn form_factor(words: &[&str]) -> Option<String> {
    let mut matched: Option<&str> = None;
    for f in FORM_FACTORS {
        if words.contains(f) {
            match matched {
                None => matched = Some(f),
                // A second, distinct form keyword: ambiguous, so bail out.
                Some(prev) if prev != *f => return None,
                Some(_) => {}
            }
        }
    }
    matched.map(str::to_string)
}

/// Every canonical [`DIETARY_FLAGS`] token whose marker phrase appears as a
/// whole-word token (sub)sequence in `words`, or `None` when none match.
///
/// Matching is token/whole-word based (not substring) for consistency with
/// form-factor detection and to avoid false positives such as "organic"
/// matching "inorganic". Multi-word markers ("gluten free", "no added sugar")
/// match a run of consecutive tokens, which also tolerates the hyphenated
/// spellings retailers use because hyphens are split into separate tokens
/// upstream ("gluten-free" tokenises to `["gluten", "free"]`).
///
/// `vegan` implies `vegetarian`: a vegan product also satisfies a vegetarian
/// superset constraint, so the derived set includes `vegetarian` whenever
/// `vegan` matches (without duplicating it if the text says both).
fn dietary_flags(words: &[&str]) -> Option<Vec<String>> {
    // (canonical flag, marker token-sequences). Each inner slice is one phrase
    // already split into the tokens it must match consecutively.
    let markers: &[(&str, &[&[&str]])] = &[
        ("gluten_free", &[&["gluten", "free"]]),
        ("dairy_free", &[&["dairy", "free"]]),
        ("lactose_free", &[&["lactose", "free"]]),
        ("vegan", &[&["vegan"]]),
        ("vegetarian", &[&["vegetarian"]]),
        ("nut_free", &[&["nut", "free"]]),
        ("organic", &[&["organic"]]),
        (
            "sugar_free",
            &[&["sugar", "free"], &["no", "added", "sugar"]],
        ),
    ];
    let mut found: Vec<String> = Vec::new();
    let push = |flag: &str, found: &mut Vec<String>| {
        if !found.iter().any(|f| f == flag) {
            found.push(flag.to_string());
        }
    };
    for (flag, phrases) in markers {
        debug_assert!(DIETARY_FLAGS.contains(flag));
        if phrases.iter().any(|p| contains_subsequence(words, p)) {
            push(flag, &mut found);
            // vegan implies vegetarian.
            if *flag == "vegan" {
                push("vegetarian", &mut found);
            }
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found)
    }
}

/// Whether `needle` appears as a run of consecutive whole-word tokens in
/// `haystack`. A single-token needle is a plain whole-word membership test.
fn contains_subsequence(haystack: &[&str], needle: &[&str]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Resolve a product's `(form_factor, dietary_flags)` preferring explicit feed
/// values over the best-effort scan of `name` + `category`.
///
/// A feed that already carries structured attributes (Backlog-T1: fixtures, or a
/// richer future source) wins; otherwise the scanners fill in what the text
/// reveals. Each attribute is resolved independently — an explicit form_factor
/// with absent dietary flags still falls back to scanning for the flags.
pub fn resolve_attributes(
    name: &str,
    category: Option<&str>,
    explicit_form: Option<String>,
    explicit_flags: Option<Vec<String>>,
) -> (Option<String>, Option<Vec<String>>) {
    let (scanned_form, scanned_flags) = extract_attributes(name, category);
    (
        explicit_form.or(scanned_form),
        explicit_flags.or(scanned_flags),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_form_factor_whole_word() {
        let (form, _) = extract_attributes("Heinz Baked Beans Can 415g", None);
        assert_eq!(form.as_deref(), Some("can"));
        // "spray" form.
        let (form, _) = extract_attributes("Mr Muscle Kitchen Spray 750ml", None);
        assert_eq!(form.as_deref(), Some("spray"));
        // No form keyword.
        let (form, _) = extract_attributes("Whole Milk 1L", None);
        assert_eq!(form, None);
    }

    #[test]
    fn ambiguous_form_factor_yields_none() {
        // Two distinct canonical forms ("can" and "tin") — refuse to guess.
        let (form, _) = extract_attributes("Old Tin Can Tomatoes 400g", None);
        assert_eq!(form, None);
        // The same form keyword repeated is not ambiguous.
        let (form, _) = extract_attributes("Can of Beans (Multipack Can)", None);
        assert_eq!(form.as_deref(), Some("can"));
    }

    #[test]
    fn extracts_dietary_flags_hyphen_and_space() {
        let (_, flags) = extract_attributes("Gluten-Free Oats 500g", None);
        assert_eq!(flags, Some(vec!["gluten_free".to_string()]));
        // vegan implies vegetarian.
        let (_, flags) = extract_attributes("Organic Vegan Dark Chocolate Bar", None);
        assert_eq!(
            flags,
            Some(vec![
                "vegan".to_string(),
                "vegetarian".to_string(),
                "organic".to_string(),
            ])
        );
        let (_, flags) = extract_attributes("Plain Digestives", None);
        assert_eq!(flags, None);
    }

    #[test]
    fn dietary_matching_is_whole_word() {
        // "inorganic" must not match the "organic" marker (substring would).
        let (_, flags) = extract_attributes("Inorganic Plant Food 1kg", None);
        assert_eq!(flags, None);
        // Whole-word "organic" still matches.
        let (_, flags) = extract_attributes("Organic Carrots 1kg", None);
        assert_eq!(flags, Some(vec!["organic".to_string()]));
    }

    #[test]
    fn vegan_does_not_duplicate_vegetarian() {
        let (_, flags) = extract_attributes("Vegan Vegetarian Sausages 300g", None);
        assert_eq!(
            flags,
            Some(vec!["vegan".to_string(), "vegetarian".to_string()])
        );
    }

    #[test]
    fn multi_word_marker_matches_token_run() {
        let (_, flags) = extract_attributes("Ketchup No Added Sugar 500ml", None);
        assert_eq!(flags, Some(vec!["sugar_free".to_string()]));
    }

    #[test]
    fn category_text_is_considered() {
        let (form, _) = extract_attributes("Coca Cola 330ml", Some("Soft Drinks Can"));
        assert_eq!(form.as_deref(), Some("can"));
    }

    #[test]
    fn explicit_values_take_precedence_over_scan() {
        // Name scans to "bottle", but an explicit "spray" wins; flags fall back to
        // the scan when not given explicitly (vegan implies vegetarian).
        let (form, flags) =
            resolve_attributes("Cleaner Bottle Vegan", None, Some("spray".into()), None);
        assert_eq!(form.as_deref(), Some("spray"));
        assert_eq!(
            flags,
            Some(vec!["vegan".to_string(), "vegetarian".to_string()])
        );
    }
}
