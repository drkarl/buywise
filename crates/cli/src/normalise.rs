//! BuyWise text normaliser (seed-generation copy).
//!
//! NOTE: duplicates the future P0-T5 normaliser; reconcile when T5 lands.
//!
//! Rules (per docs/design.md "Text normalisation rules"): lowercase, NFD accent
//! strip, punctuation -> SPACE (NOT deletion — `"coca-cola"` -> `"coca cola"`,
//! deliberately differing from Shoppy which deletes punctuation), whitespace
//! collapse, trim. Exact normalisation only — no typo/fuzzy logic.

use unicode_normalization::UnicodeNormalization;

/// Normalise `text` for alias storage/matching.
///
/// 1. lowercase
/// 2. NFD decomposition, then drop combining marks (U+0300..=U+036F)
/// 3. replace every non-alphanumeric, non-space char with a space
/// 4. collapse runs of whitespace to a single space, trim ends
pub fn normalise(text: &str) -> String {
    let lowered = text.to_lowercase();

    // NFD decompose and strip combining diacritical marks.
    let stripped: String = lowered
        .nfd()
        .filter(|c| !('\u{0300}'..='\u{036f}').contains(c))
        .collect();

    // Punctuation -> space (keep alphanumerics and whitespace).
    let spaced: String = stripped
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();

    // Collapse whitespace and trim.
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::normalise;

    #[test]
    fn lowercases_and_trims() {
        assert_eq!(normalise("  LECHE  "), "leche");
    }

    #[test]
    fn strips_accents() {
        assert_eq!(normalise("Jamón"), "jamon");
        assert_eq!(normalise("Plátano"), "platano");
    }

    #[test]
    fn punctuation_becomes_space_not_deleted() {
        // The defining BuyWise difference from Shoppy.
        assert_eq!(normalise("coca-cola"), "coca cola");
        assert_eq!(normalise("M&M's"), "m m s");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(normalise("multiple   spaces"), "multiple spaces");
    }

    #[test]
    fn empty_input() {
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("   "), "");
    }
}
