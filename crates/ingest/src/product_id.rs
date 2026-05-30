//! Deterministic primary keys for ingested retailer products.
//!
//! `retailer_products.id` is **not** a random UUID: it is a SHA-256 hash of the
//! `(source, retailer, external_id)` triple. Determinism is load-bearing —
//! `fixtures/product_matches.csv` hardcodes `retailer_product_id` values, so the
//! same product fetched into a fresh database must always receive the same id.
//! See the "Shared ingest rule" in `docs/implementation-plan.md` (above P0-T8).
//!
//! Both the BasketWatch (P0-T8) and Lidl (P0-T9) adapters use this helper so the
//! hashing logic lives in exactly one place.

use buywise_core::{Retailer, Source};
use sha2::{Digest, Sha256};

/// Deterministic `retailer_products.id` for a non-manual source.
///
/// Computed as `hex(SHA-256("<source>:<retailer>:<external_id>"))` where the
/// source/retailer components are their canonical DB strings (via
/// [`Source::as_db_str`] / [`Retailer::as_db_str`]). The `external_id` is
/// trimmed of surrounding whitespace but **not** lowercased — retailers may use
/// case-sensitive identifiers, so `"AB12"` and `"ab12"` are distinct products.
///
/// Taking typed enums (rather than raw strings) means a caller cannot pass a
/// mismatched source/retailer value, and a renamed enum variant cannot silently
/// change the hash.
pub fn stable_product_id(source: Source, retailer: Retailer, external_id: &str) -> String {
    let key = format!(
        "{}:{}:{}",
        source.as_db_str(),
        retailer.as_db_str(),
        external_id.trim()
    );
    hex::encode(Sha256::digest(key.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic() {
        let a = stable_product_id(Source::BasketWatch, Retailer::Tesco, "12345");
        let b = stable_product_id(Source::BasketWatch, Retailer::Tesco, "12345");
        assert_eq!(a, b);
        // SHA-256 hex is 64 chars.
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn trims_external_id_but_preserves_case() {
        // Surrounding whitespace is irrelevant…
        assert_eq!(
            stable_product_id(Source::BasketWatch, Retailer::Aldi, "  ext-1  "),
            stable_product_id(Source::BasketWatch, Retailer::Aldi, "ext-1"),
        );
        // …but case is significant.
        assert_ne!(
            stable_product_id(Source::BasketWatch, Retailer::Aldi, "AB12"),
            stable_product_id(Source::BasketWatch, Retailer::Aldi, "ab12"),
        );
    }

    #[test]
    fn distinguishes_source_retailer_and_external_id() {
        let base = stable_product_id(Source::BasketWatch, Retailer::Tesco, "x");
        assert_ne!(
            base,
            stable_product_id(Source::LidlDirect, Retailer::Tesco, "x"),
            "source must affect the hash"
        );
        assert_ne!(
            base,
            stable_product_id(Source::BasketWatch, Retailer::Dunnes, "x"),
            "retailer must affect the hash"
        );
        assert_ne!(
            base,
            stable_product_id(Source::BasketWatch, Retailer::Tesco, "y"),
            "external_id must affect the hash"
        );
    }

    /// Guards the exact wire format. If the separator or component order ever
    /// changes, every persisted id and every hardcoded `product_matches.csv`
    /// value would be invalidated — so pin the known-good digest.
    #[test]
    fn matches_known_digest() {
        // hex(SHA-256("basketwatch:tesco:12345"))
        let expected = {
            let key = "basketwatch:tesco:12345";
            hex::encode(Sha256::digest(key.as_bytes()))
        };
        assert_eq!(
            stable_product_id(Source::BasketWatch, Retailer::Tesco, "12345"),
            expected
        );
    }
}
