//! BuyWise ingest adapters.
//!
//! Fetches retailer price data from external sources and normalises it into the
//! `retailer_products` + `product_offers` tables. The BasketWatch RapidAPI
//! adapter ([`basketwatch`]) covers Tesco, Dunnes Stores, SuperValu and Aldi;
//! the Lidl adapter ([`lidl`], P0-T9) covers Lidl.ie directly and shares
//! [`product_id`]. The Lidl run is isolated from BasketWatch via
//! [`lidl::spawn_ingest`], so a Lidl failure cannot fail the BasketWatch run.

pub mod attributes;
pub mod basketwatch;
pub mod error;
pub mod lidl;
pub mod product_id;

/// Read an environment variable, treating an unset, empty, or whitespace-only
/// value as "not configured" (`None`).
///
/// `std::env::var` returns `Ok("")` for a variable that is *set but empty* —
/// which is exactly how systemd's `EnvironmentFile` exports a bare `VAR=` line
/// (as the deploy env template seeds the optional ingest keys). The ingest
/// adapters require an unset key/URL to fail fast as a `Missing*` error and an
/// empty one must take the same path, so both collapse to `None` here.
pub(crate) fn configured_env(name: &str) -> Option<String> {
    // The env read is a thin shim; the "is this actually configured?" rule lives
    // in `normalize_configured` so it can be unit-tested without mutating
    // process-global environment state (which is racy under parallel tests).
    normalize_configured(std::env::var(name).ok())
}

/// Collapse a raw environment lookup to `None` when the value is absent, empty,
/// or whitespace-only. A present, non-blank value is returned verbatim (callers
/// that need trimming do it themselves).
fn normalize_configured(raw: Option<String>) -> Option<String> {
    raw.filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod env_tests {
    use super::normalize_configured;

    #[test]
    fn unset_empty_and_whitespace_collapse_to_none() {
        assert_eq!(normalize_configured(None), None, "unset is not configured");
        assert_eq!(
            normalize_configured(Some(String::new())),
            None,
            "empty string is not configured"
        );
        assert_eq!(
            normalize_configured(Some("   \t ".to_string())),
            None,
            "whitespace-only string is not configured"
        );
    }

    #[test]
    fn real_value_passes_through_verbatim() {
        assert_eq!(
            normalize_configured(Some("  value  ".to_string())),
            Some("  value  ".to_string()),
            "a non-blank value is returned without trimming"
        );
    }
}
