//! BuyWise CLI library.
//!
//! Houses the deterministic seed-fixture generator behind the `generate-fixtures`
//! subcommand. The generator reads `fixtures/dictionary.json` (the single source
//! of truth, ported from Shoppy) and emits two byte-stable artefacts:
//!
//!   * `fixtures/canonical_items.json` — generated canonical items with computed
//!     IDs plus their alias rows.
//!   * `migrations/002_seed.sql` — the seed migration (categories +
//!     canonical_items + item_aliases INSERTs).
//!
//! Determinism is guaranteed by: SHA-256-derived IDs (no UUID v4), a fixed
//! `created_at` constant (never `now()`), and stable output ordering. Re-running
//! the generator produces byte-identical files.

pub mod generate;
pub mod model;
pub mod normalise;
