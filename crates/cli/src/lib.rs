//! BuyWise CLI library.
//!
//! Hosts the Phase-0 developer CLI: the deterministic seed-fixture generator
//! ([`generate`]) plus the subcommands in [`commands`] (validate-basket,
//! optimise, ingest-basketwatch, ingest-lidl, seed-matches, generate-fixtures).
//!
//! The seed generator reads `fixtures/dictionary.json` (the single source of
//! truth, ported from Shoppy) and emits two byte-stable artefacts:
//!
//!   * `fixtures/canonical_items.json` — generated canonical items with computed
//!     IDs plus their alias rows.
//!   * `migrations/002_seed.sql` — the seed migration (categories +
//!     canonical_items + item_aliases INSERTs).
//!
//! Determinism is guaranteed by: SHA-256-derived IDs (no UUID v4), a fixed
//! `created_at` constant (never `now()`), and stable output ordering. Re-running
//! the generator produces byte-identical files.
//!
//! The command modules are thin orchestrators: they parse fixtures, build the
//! `OptimiserInput` pre-step (count/size parsing, constraint merge), and call
//! the existing `crates/core` and `crates/ingest` APIs. No domain logic is
//! re-implemented here.

pub mod commands;
pub mod generate;
pub mod model;
pub mod normalise;

/// Workspace root resolved from this crate's manifest dir (`../../`).
///
/// Used by the command modules to locate the committed fixtures and migrations
/// regardless of the process CWD, mirroring [`generate`]'s own resolution.
pub fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}
