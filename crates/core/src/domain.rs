use serde::{Deserialize, Serialize};

pub struct Item;

// Price amounts — never use f64 for money (see design §9, implementation-plan line 17).
/// Price in euro cents (price × 100).
pub type Cents = i64;
/// Unit price in micros (price × 1_000_000).
pub type Micros = i64;

/// Base unit a normalised size is expressed in. Input units ("L", "kg", "cl")
/// are converted to these during parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeBasis {
    Grams,
    Millilitres,
    Each,
}

/// The unit a product's price is normalised against for cross-product comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// A size constraint for product matching, expressed in the normalised SI value
/// (grams for weight, millilitres for volume, count for `Each`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RequestedSize {
    /// Normalised value: grams for weight, millilitres for volume, count for `Each`.
    pub value: f64,
    /// Input units ("L", "kg", "cl") are converted during parsing,
    /// e.g. "1L" → `{ value: 1000.0, basis: Millilitres }`.
    pub basis: SizeBasis,
}
