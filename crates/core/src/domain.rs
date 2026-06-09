//! Core domain types shared across the workspace.
//!
//! These are plain data carriers: row structs decoded from SQLite via
//! [`sqlx::FromRow`], the enums they embed, and a couple of parsing helpers
//! ([`ParsedInput`], [`RequestedSize`]) consumed by `normalise.rs`.
//!
//! # Money
//!
//! Prices are integers, never floats: [`Cents`] holds a price in euro cents
//! (`price × 100`) and [`Micros`] holds a unit price in micros
//! (`price × 1_000_000`). Both are `i64` aliases so that money never enters
//! floating-point arithmetic.
//!
//! # Enum ↔ database mapping
//!
//! Every enum that is persisted as TEXT serialises to the exact string used by
//! the `CHECK` constraints in `migrations/001_phase0_schema.sql`. The mapping is
//! deliberately spelled out per variant rather than relying on a uniform
//! `rename_all` rule, because it is *not* uniform — e.g. [`Source::BasketWatch`]
//! is `"basketwatch"` (no underscore) while [`Source::LidlDirect`] is
//! `"lidl_direct"` (with one). Each DB-backed enum exposes:
//!
//! * [`as_db_str`](Source::as_db_str) — the canonical TEXT value (also used by
//!   `stable_product_id()` and every DB write), and
//! * `from_db_str` / [`TryFrom<&str>`] — the inverse, with explicit match arms.
//!
//! `serde` derives are configured to agree with `as_db_str` so JSON and DB
//! representations are identical. The `sqlx::Type`/`Decode`/`Encode` impls are
//! hand-written on top of `as_db_str`/`from_db_str` so the non-uniform mappings
//! (notably [`Source`]) decode and encode correctly for SQLite.

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteArgumentValue, SqliteTypeInfo, SqliteValueRef};
use sqlx::{Decode, Encode, Sqlite, Type};

/// Price in euro cents (`price × 100`). Never use `f64` for money.
pub type Cents = i64;

/// Unit price in micros (`price × 1_000_000`). Never use `f64` for money.
pub type Micros = i64;

/// Error returned when a TEXT value does not map to a known enum variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} value: {value:?}")]
pub struct ParseEnumError {
    /// The enum type that failed to parse (e.g. `"Source"`).
    pub kind: &'static str,
    /// The offending input string.
    pub value: String,
}

/// Generate the shared boilerplate for a TEXT-backed enum:
///
/// * `as_db_str` / `from_db_str` / `TryFrom<&str>` using explicit per-variant
///   strings, and
/// * hand-written `sqlx::Type<Sqlite>` + `Decode` + `Encode` so SQLite round-trips
///   through the exact DB string regardless of variant-name shape.
///
/// `serde` is derived separately on each enum so the rename rules stay visible
/// at the definition site.
macro_rules! text_enum_db {
    ($name:ident { $($variant:ident => $db:literal),+ $(,)? }) => {
        impl $name {
            /// The canonical TEXT value used by the DB `CHECK` constraints,
            /// `stable_product_id()`, and all DB writes.
            pub fn as_db_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $db, )+
                }
            }

            /// Parse from the canonical DB TEXT value.
            pub fn from_db_str(value: &str) -> Result<Self, ParseEnumError> {
                match value {
                    $( $db => Ok($name::$variant), )+
                    other => Err(ParseEnumError {
                        kind: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ParseEnumError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                $name::from_db_str(value)
            }
        }

        impl Type<Sqlite> for $name {
            fn type_info() -> SqliteTypeInfo {
                <String as Type<Sqlite>>::type_info()
            }
            fn compatible(ty: &SqliteTypeInfo) -> bool {
                <String as Type<Sqlite>>::compatible(ty)
            }
        }

        impl<'q> Encode<'q, Sqlite> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut Vec<SqliteArgumentValue<'q>>,
            ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
                <&str as Encode<Sqlite>>::encode(self.as_db_str(), buf)
            }
        }

        impl<'r> Decode<'r, Sqlite> for $name {
            fn decode(value: SqliteValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
                let text = <&str as Decode<Sqlite>>::decode(value)?;
                Ok($name::from_db_str(text)?)
            }
        }
    };
}

/// Retailers BuyWise tracks. DB TEXT: `tesco`, `dunnes`, `supervalu`, `aldi`, `lidl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Retailer {
    Tesco,
    Dunnes,
    SuperValu,
    Aldi,
    Lidl,
}

text_enum_db!(Retailer {
    Tesco => "tesco",
    Dunnes => "dunnes",
    SuperValu => "supervalu",
    Aldi => "aldi",
    Lidl => "lidl",
});

/// Origin of a retailer product / offer. DB TEXT mapping is non-uniform:
/// `BasketWatch` → `basketwatch`, `LidlDirect` → `lidl_direct`, `Manual` → `manual`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    #[serde(rename = "basketwatch")]
    BasketWatch,
    #[serde(rename = "lidl_direct")]
    LidlDirect,
    #[serde(rename = "manual")]
    Manual,
}

text_enum_db!(Source {
    BasketWatch => "basketwatch",
    LidlDirect => "lidl_direct",
    Manual => "manual",
});

/// Resolution state of a parsed list entry (Phase 1; no Phase-0 `CHECK`).
/// TEXT: `unresolved`, `resolved`, `ambiguous`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolutionStatus {
    Unresolved,
    Resolved,
    Ambiguous,
}

text_enum_db!(ResolutionStatus {
    Unresolved => "unresolved",
    Resolved => "resolved",
    Ambiguous => "ambiguous",
});

/// Lifecycle state of a list item (Phase 1; no Phase-0 `CHECK`).
/// TEXT: `active`, `bought`, `skipped`, `deleted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    Active,
    Bought,
    Skipped,
    Deleted,
}

text_enum_db!(ItemStatus {
    Active => "active",
    Bought => "bought",
    Skipped => "skipped",
    Deleted => "deleted",
});

/// State of a product match. DB TEXT: `accepted`, `rejected`, `suggested`, `pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchStatus {
    Accepted,
    Rejected,
    Suggested,
    Pending,
}

text_enum_db!(MatchStatus {
    Accepted => "accepted",
    Rejected => "rejected",
    Suggested => "suggested",
    Pending => "pending",
});

/// Confidence in a source at ingest time. DB TEXT: `high`, `medium`, `low`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceConfidence {
    High,
    Medium,
    Low,
}

text_enum_db!(SourceConfidence {
    High => "high",
    Medium => "medium",
    Low => "low",
});

/// Confidence in data freshness (Phase 1; no Phase-0 `CHECK`).
/// TEXT: `high`, `low`, `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FreshnessConfidence {
    High,
    Low,
    Unknown,
}

text_enum_db!(FreshnessConfidence {
    High => "high",
    Low => "low",
    Unknown => "unknown",
});

/// How strictly a canonical item constrains matching products.
/// DB TEXT (snake_case): `any`, `brand`, `allowed_set`, `own_label`, `exact`, `ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintPolicy {
    Any,
    Brand,
    AllowedSet,
    OwnLabel,
    Exact,
    Ask,
}

text_enum_db!(ConstraintPolicy {
    Any => "any",
    Brand => "brand",
    AllowedSet => "allowed_set",
    OwnLabel => "own_label",
    Exact => "exact",
    Ask => "ask",
});

/// Whether substitutions are permitted for an item. DB TEXT: `any`, `ask`, `strict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubstitutionPolicy {
    Any,
    Ask,
    Strict,
}

text_enum_db!(SubstitutionPolicy {
    Any => "any",
    Ask => "ask",
    Strict => "strict",
});

/// SI basis for a normalised size. DB TEXT (`item_constraints` size columns):
/// `Grams` → `g`, `Millilitres` → `ml`, `Each` → `each`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SizeBasis {
    #[serde(rename = "g")]
    Grams,
    #[serde(rename = "ml")]
    Millilitres,
    #[serde(rename = "each")]
    Each,
}

text_enum_db!(SizeBasis {
    Grams => "g",
    Millilitres => "ml",
    Each => "each",
});

/// Basis a unit price is quoted against. DB TEXT (snake_case): `per_kg`, `per_l`,
/// `per_each`, `per_sheet`, `per_nappy`, `per_wash`, `per_pod`, `per_roll`.
///
/// `product_offers.unit_price_basis` is free TEXT (no `CHECK`); these canonical
/// strings are used regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitPriceBasis {
    PerKg,
    PerL,
    PerEach,
    PerSheet,
    PerNappy,
    PerWash,
    PerPod,
    PerRoll,
}

text_enum_db!(UnitPriceBasis {
    PerKg => "per_kg",
    PerL => "per_l",
    PerEach => "per_each",
    PerSheet => "per_sheet",
    PerNappy => "per_nappy",
    PerWash => "per_wash",
    PerPod => "per_pod",
    PerRoll => "per_roll",
});

/// A normalised size constraint produced during parsing.
///
/// `value` is the SI value: grams for weight, millilitres for volume, count for
/// [`SizeBasis::Each`]. Input units (`"L"`, `"kg"`, `"cl"`) are converted while
/// parsing — e.g. `"1L"` becomes `{ value: 1000.0, basis: Millilitres }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestedSize {
    pub value: f64,
    pub basis: SizeBasis,
}

/// Result of parsing a raw shopping-list line into structured form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedInput {
    /// Normalised text with count/size stripped.
    pub text: String,
    /// How many items to buy (default 1).
    pub count: u32,
    /// Size constraint for product matching; not multiplied by price.
    pub requested_size: Option<RequestedSize>,
    /// The original, unmodified input line.
    pub original: String,
}

/// A product category. Row of `categories`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Category {
    pub id: String,
    pub name_en: String,
    pub name_es: String,
    pub icon: String,
    pub default_order: i32,
}

/// A household's canonical item. Row of `canonical_items`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct CanonicalItem {
    pub id: String,
    pub household_id: String,
    pub name_en: String,
    pub name_es: String,
    pub category_id: String,
    pub default_constraint_policy: ConstraintPolicy,
    pub created_at: String,
}

/// An alias pointing at a canonical item. Row of `item_aliases`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ItemAlias {
    pub id: String,
    pub household_id: String,
    pub canonical_item_id: String,
    pub alias: String,
    pub language: String,
    /// One of `"manual"`, `"accepted_match"`, `"llm"`.
    pub source: String,
    pub created_at: String,
}

/// Matching constraints for a canonical item. Row of `item_constraints`.
///
/// `allowed_brands`, `excluded_brands` and `dietary_flags` are stored as JSON
/// TEXT columns. A [`sqlx::FromRow`] derive is intentionally deferred: it would
/// require the sqlx `json` feature (not enabled in the workspace) to decode the
/// `Option<Vec<String>>` columns cleanly, and no current read path needs it.
/// `serde` round-trips fully today.
// TODO(P1): derive FromRow once a read path needs it (enable sqlx `json`
// feature or hand-write FromRow to parse the JSON TEXT columns).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemConstraints {
    pub id: String,
    pub canonical_item_id: String,
    pub brand: Option<String>,
    pub allowed_brands: Option<Vec<String>>,
    pub excluded_brands: Option<Vec<String>>,
    pub preferred_size_value: Option<f64>,
    pub preferred_size_basis: Option<SizeBasis>,
    pub min_size_value: Option<f64>,
    pub min_size_basis: Option<SizeBasis>,
    pub max_size_value: Option<f64>,
    pub max_size_basis: Option<SizeBasis>,
    pub pack_count: Option<i32>,
    pub form_factor: Option<String>,
    pub dietary_flags: Option<Vec<String>>,
    pub substitution_policy: SubstitutionPolicy,
    pub updated_at: String,
}

/// Canonical `retailer_products.form_factor` vocabulary (Backlog-T1).
///
/// `form_factor` is the physical product form, stored as a single lowercase
/// snake/kebab token in both `item_constraints.form_factor` (the constraint) and
/// `retailer_products.form_factor` (the product attribute). Comparison in the
/// optimiser is case-insensitive. Ingest populates the column best-effort from
/// product name/category keywords; an unrecognised or absent form is `NULL`.
///
/// The vocabulary is intentionally small and closed so constraints and products
/// agree on spelling:
/// `can`, `bottle`, `carton`, `pouch`, `jar`, `tub`, `bar`, `spray`, `box`,
/// `bag`, `tin`, `tetra`.
pub const FORM_FACTORS: &[&str] = &[
    "can", "bottle", "carton", "pouch", "jar", "tub", "bar", "spray", "box", "bag", "tin", "tetra",
];

/// Canonical `retailer_products.dietary_flags` vocabulary (Backlog-T1).
///
/// `dietary_flags` is a set of lowercase snake_case dietary attribute tokens,
/// stored as a JSON array TEXT column on both `item_constraints.dietary_flags`
/// (the required set) and `retailer_products.dietary_flags` (what the product
/// satisfies). A product qualifies for a constraint only if its flag set is a
/// **superset** of the required flags. Ingest populates the column best-effort
/// from product attributes; products with no detectable flags get `NULL` (an
/// empty/absent set satisfies only an empty/absent constraint).
pub const DIETARY_FLAGS: &[&str] = &[
    "gluten_free",
    "dairy_free",
    "lactose_free",
    "vegan",
    "vegetarian",
    "nut_free",
    "organic",
    "sugar_free",
];

/// A product as seen at a retailer. Row of `retailer_products`.
///
/// `dietary_flags` is stored as a JSON array TEXT column. A hand-written
/// [`sqlx::FromRow`] (below) decodes it, because the derive cannot map a JSON
/// TEXT column into `Option<Vec<String>>` without the sqlx `json` feature (not
/// enabled in the workspace). `form_factor` is a plain TEXT token. All other
/// columns decode by name exactly as the former derive did. See the
/// [`FORM_FACTORS`] / [`DIETARY_FLAGS`] vocabularies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetailerProduct {
    pub id: String,
    pub source: Source,
    pub retailer: Retailer,
    pub external_id: Option<String>,
    pub url: Option<String>,
    pub name: String,
    pub brand: Option<String>,
    pub category: Option<String>,
    pub pack_size_text: Option<String>,
    pub size_value: Option<f64>,
    pub size_unit: Option<String>,
    pub pack_count: Option<i32>,
    /// Physical form of the product (see [`FORM_FACTORS`]); `None` when unknown.
    pub form_factor: Option<String>,
    /// Dietary attributes the product satisfies (see [`DIETARY_FLAGS`]), decoded
    /// from a JSON array TEXT column; `None` when unknown.
    pub dietary_flags: Option<Vec<String>>,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for RetailerProduct {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        // `dietary_flags` is a JSON-array TEXT column; decode the raw TEXT then
        // parse it. A NULL column → `None`; a present-but-unparseable value is a
        // data error surfaced as a decode failure (never silently dropped).
        let dietary_flags_raw: Option<String> = row.try_get("dietary_flags")?;
        let dietary_flags = match dietary_flags_raw {
            Some(s) => Some(serde_json::from_str::<Vec<String>>(&s).map_err(|e| {
                sqlx::Error::ColumnDecode {
                    index: "dietary_flags".to_string(),
                    source: Box::new(e),
                }
            })?),
            None => None,
        };
        Ok(RetailerProduct {
            id: row.try_get("id")?,
            source: row.try_get("source")?,
            retailer: row.try_get("retailer")?,
            external_id: row.try_get("external_id")?,
            url: row.try_get("url")?,
            name: row.try_get("name")?,
            brand: row.try_get("brand")?,
            category: row.try_get("category")?,
            pack_size_text: row.try_get("pack_size_text")?,
            size_value: row.try_get("size_value")?,
            size_unit: row.try_get("size_unit")?,
            pack_count: row.try_get("pack_count")?,
            form_factor: row.try_get("form_factor")?,
            dietary_flags,
            first_seen_at: row.try_get("first_seen_at")?,
            last_seen_at: row.try_get("last_seen_at")?,
        })
    }
}

/// A point-in-time price/offer for a retailer product. Row of `product_offers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProductOffer {
    pub id: String,
    pub retailer_product_id: String,
    pub price_cents: Cents,
    pub unit_price_eur_micros: Option<Micros>,
    pub unit_price_basis: Option<UnitPriceBasis>,
    pub previous_price_cents: Option<Cents>,
    pub loyalty_price_cents: Option<Cents>,
    pub loyalty_scheme: Option<String>,
    pub offer_text: Option<String>,
    pub offer_valid_until: Option<String>,
    pub source_snapshot_date: Option<String>,
    pub fetched_at: String,
    pub cache_expires_at: Option<String>,
    pub last_successful_source_run_id: Option<String>,
    pub source_successful_run_number_at_fetch: Option<i64>,
    pub source_confidence_at_ingest: SourceConfidence,
}

/// A bookkeeping record for one ingest run. Row of `source_runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct SourceRun {
    pub id: String,
    pub source: Source,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub requests_made: i32,
    pub items_fetched: i32,
    pub items_normalised: i32,
    /// One of `"running"`, `"success"`, `"failed"`.
    pub status: String,
    pub error_message: Option<String>,
    pub successful_run_number: Option<i64>,
}

/// A link between a canonical item and a retailer product. Row of `product_matches`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProductMatch {
    pub id: String,
    pub household_id: String,
    pub canonical_item_id: String,
    pub retailer_product_id: String,
    pub status: MatchStatus,
    /// One of `"user"`, `"system"`, `"llm"`.
    pub matched_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn sample_offer() -> ProductOffer {
        ProductOffer {
            id: "offer-1".into(),
            retailer_product_id: "rp-1".into(),
            price_cents: 199,
            unit_price_eur_micros: Some(1_990_000),
            unit_price_basis: Some(UnitPriceBasis::PerKg),
            previous_price_cents: Some(249),
            loyalty_price_cents: Some(149),
            loyalty_scheme: Some("clubcard".into()),
            offer_text: Some("Clubcard Price".into()),
            offer_valid_until: Some("2026-06-30".into()),
            source_snapshot_date: Some("2026-05-29".into()),
            fetched_at: "2026-05-29T10:00:00Z".into(),
            cache_expires_at: Some("2026-05-30T10:00:00Z".into()),
            last_successful_source_run_id: Some("run-1".into()),
            source_successful_run_number_at_fetch: Some(42),
            source_confidence_at_ingest: SourceConfidence::High,
        }
    }

    #[test]
    fn product_offer_serde_round_trip() {
        let offer = sample_offer();
        let json = serde_json::to_string(&offer).expect("serialize");
        let back: ProductOffer = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(offer, back);
    }

    #[test]
    fn retailer_product_serde_round_trip() {
        let product = RetailerProduct {
            id: "rp-1".into(),
            source: Source::LidlDirect,
            retailer: Retailer::Lidl,
            external_id: Some("ext-1".into()),
            url: Some("https://example.test/p/1".into()),
            name: "Whole Milk 1L".into(),
            brand: Some("Lidl".into()),
            category: Some("dairy".into()),
            pack_size_text: Some("1L".into()),
            size_value: Some(1000.0),
            size_unit: Some("ml".into()),
            pack_count: Some(1),
            form_factor: Some("carton".into()),
            dietary_flags: Some(vec!["lactose_free".into()]),
            first_seen_at: "2026-05-01T00:00:00Z".into(),
            last_seen_at: "2026-05-29T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&product).expect("serialize");
        // Source must serialise to its DB string, not the variant name.
        assert!(json.contains("\"lidl_direct\""));
        let back: RetailerProduct = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(product, back);
    }

    #[test]
    fn canonical_item_serde_round_trip() {
        let item = CanonicalItem {
            id: "ci-1".into(),
            household_id: "hh-1".into(),
            name_en: "Milk".into(),
            name_es: "Leche".into(),
            category_id: "dairy".into(),
            default_constraint_policy: ConstraintPolicy::OwnLabel,
            created_at: "2026-05-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(json.contains("\"own_label\""));
        let back: CanonicalItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item, back);
    }

    #[test]
    fn item_constraints_serde_round_trip_with_json_vecs() {
        let constraints = ItemConstraints {
            id: "ic-1".into(),
            canonical_item_id: "ci-1".into(),
            brand: Some("Tesco".into()),
            allowed_brands: Some(vec!["Tesco".into(), "Avonmore".into()]),
            excluded_brands: Some(vec!["Generic".into()]),
            preferred_size_value: Some(1000.0),
            preferred_size_basis: Some(SizeBasis::Millilitres),
            min_size_value: Some(500.0),
            min_size_basis: Some(SizeBasis::Millilitres),
            max_size_value: Some(2000.0),
            max_size_basis: Some(SizeBasis::Millilitres),
            pack_count: Some(1),
            form_factor: Some("carton".into()),
            dietary_flags: Some(vec!["lactose_free".into()]),
            substitution_policy: SubstitutionPolicy::Ask,
            updated_at: "2026-05-29T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&constraints).expect("serialize");
        assert!(json.contains("\"ml\""));
        let back: ItemConstraints = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(constraints, back);
    }

    #[test]
    fn enum_db_string_mappings() {
        // Non-uniform Source mapping.
        assert_eq!(Source::BasketWatch.as_db_str(), "basketwatch");
        assert_eq!(Source::LidlDirect.as_db_str(), "lidl_direct");
        assert_eq!(Source::Manual.as_db_str(), "manual");
        // Retailer (SuperValu collapses to "supervalu").
        assert_eq!(Retailer::SuperValu.as_db_str(), "supervalu");
        // snake_case policies.
        assert_eq!(ConstraintPolicy::OwnLabel.as_db_str(), "own_label");
        assert_eq!(ConstraintPolicy::AllowedSet.as_db_str(), "allowed_set");
        assert_eq!(UnitPriceBasis::PerKg.as_db_str(), "per_kg");
        // SizeBasis abbreviations.
        assert_eq!(SizeBasis::Grams.as_db_str(), "g");
        assert_eq!(SizeBasis::Millilitres.as_db_str(), "ml");
        assert_eq!(SizeBasis::Each.as_db_str(), "each");

        // Round-trip via from_db_str / TryFrom.
        assert_eq!(
            Source::from_db_str("lidl_direct").unwrap(),
            Source::LidlDirect
        );
        assert_eq!(
            Source::from_db_str("basketwatch").unwrap(),
            Source::BasketWatch
        );
        assert_eq!(
            Retailer::try_from("supervalu").unwrap(),
            Retailer::SuperValu
        );
        assert_eq!(
            ConstraintPolicy::try_from("own_label").unwrap(),
            ConstraintPolicy::OwnLabel
        );
        assert_eq!(
            UnitPriceBasis::try_from("per_kg").unwrap(),
            UnitPriceBasis::PerKg
        );

        // Unknown values error rather than silently mapping.
        assert!(Source::from_db_str("lidldirect").is_err());
        assert!(Retailer::from_db_str("supermarket").is_err());
    }

    #[test]
    fn serde_and_db_string_agree() {
        // The serde form must equal the DB string for every DB-backed enum, so a
        // value serialised to JSON can be read back from a TEXT column unchanged.
        for s in [Source::BasketWatch, Source::LidlDirect, Source::Manual] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_db_str()));
        }
        for r in [
            Retailer::Tesco,
            Retailer::Dunnes,
            Retailer::SuperValu,
            Retailer::Aldi,
            Retailer::Lidl,
        ] {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(json, format!("\"{}\"", r.as_db_str()));
        }
    }

    /// Proves `FromRow` + the hand-written enum `Decode` impls compile and run
    /// for `RetailerProduct` and `ProductOffer` against a real SQLite pool.
    /// Zero rows is the expected (and sufficient) result.
    #[tokio::test]
    async fn query_as_compiles_for_row_structs() {
        let pool = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");

        let products = sqlx::query_as::<_, RetailerProduct>("SELECT * FROM retailer_products")
            .fetch_all(&pool)
            .await
            .expect("query retailer_products");
        assert_eq!(products.len(), 0);

        let offers = sqlx::query_as::<_, ProductOffer>("SELECT * FROM product_offers")
            .fetch_all(&pool)
            .await
            .expect("query product_offers");
        assert_eq!(offers.len(), 0);
    }

    /// Reads the 11 seeded categories via `query_as` to prove `FromRow` works
    /// against real data.
    #[tokio::test]
    async fn query_as_reads_seeded_categories() {
        let pool = db::connect("sqlite::memory:").await.expect("connect");
        db::run_migrations(&pool).await.expect("migrate");

        let categories = sqlx::query_as::<_, Category>("SELECT * FROM categories")
            .fetch_all(&pool)
            .await
            .expect("query categories");
        assert_eq!(categories.len(), 11);
    }
}
