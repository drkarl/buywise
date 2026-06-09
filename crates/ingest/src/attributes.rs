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
/// Matching is whole-word and case-insensitive. A form factor is emitted only
/// when a single canonical form keyword is present; dietary flags accumulate
/// every canonical flag whose marker phrase appears. Absent signals yield
/// `None` (an empty dietary set is represented as `None`, not `Some(vec![])`,
/// so it satisfies only an empty constraint).
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
    let has_word = |w: &str| words.contains(&w);

    (form_factor(&has_word), dietary_flags(&haystack))
}

/// First canonical [`FORM_FACTORS`] token that appears as a whole word, or
/// `None`. "spray" and "can" etc. are unambiguous physical forms.
fn form_factor(has_word: &impl Fn(&str) -> bool) -> Option<String> {
    FORM_FACTORS
        .iter()
        .find(|f| has_word(f))
        .map(|f| (*f).to_string())
}

/// Every canonical [`DIETARY_FLAGS`] token whose marker phrase appears in the
/// (lowercased) haystack, or `None` when none match. Markers tolerate the
/// hyphenated/spaced spellings retailers use ("gluten free", "gluten-free").
fn dietary_flags(haystack: &str) -> Option<Vec<String>> {
    // (canonical flag, marker phrases). The haystack is lowercased; we match the
    // space form because punctuation is not normalised here — instead we also
    // check a punctuation-collapsed copy so "gluten-free" matches "gluten free".
    let collapsed: String = haystack
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let markers: &[(&str, &[&str])] = &[
        ("gluten_free", &["gluten free"]),
        ("dairy_free", &["dairy free"]),
        ("lactose_free", &["lactose free"]),
        ("vegan", &["vegan"]),
        ("vegetarian", &["vegetarian"]),
        ("nut_free", &["nut free"]),
        ("organic", &["organic"]),
        ("sugar_free", &["sugar free", "no added sugar"]),
    ];
    let mut found = Vec::new();
    for (flag, phrases) in markers {
        debug_assert!(DIETARY_FLAGS.contains(flag));
        if phrases.iter().any(|p| collapsed.contains(p)) {
            found.push((*flag).to_string());
        }
    }
    if found.is_empty() {
        None
    } else {
        Some(found)
    }
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
    fn extracts_dietary_flags_hyphen_and_space() {
        let (_, flags) = extract_attributes("Gluten-Free Oats 500g", None);
        assert_eq!(flags, Some(vec!["gluten_free".to_string()]));
        let (_, flags) = extract_attributes("Organic Vegan Dark Chocolate Bar", None);
        assert_eq!(
            flags,
            Some(vec!["vegan".to_string(), "organic".to_string(),])
        );
        let (_, flags) = extract_attributes("Plain Digestives", None);
        assert_eq!(flags, None);
    }

    #[test]
    fn category_text_is_considered() {
        let (form, _) = extract_attributes("Coca Cola 330ml", Some("Soft Drinks Can"));
        assert_eq!(form.as_deref(), Some("can"));
    }

    #[test]
    fn explicit_values_take_precedence_over_scan() {
        // Name scans to "bottle", but an explicit "spray" wins; flags fall back to
        // the scan when not given explicitly.
        let (form, flags) =
            resolve_attributes("Cleaner Bottle Vegan", None, Some("spray".into()), None);
        assert_eq!(form.as_deref(), Some("spray"));
        assert_eq!(flags, Some(vec!["vegan".to_string()]));
    }
}
