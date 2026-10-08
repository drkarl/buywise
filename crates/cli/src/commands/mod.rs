//! Phase-0 CLI subcommands.
//!
//! Each submodule is a thin orchestrator over the existing `crates/core` and
//! `crates/ingest` APIs. Shared plumbing — the basket fixture format, the
//! resolution + constraint-merge pre-step, and small money/printing helpers —
//! lives here in [`mod@self`]. The hidden password prompt shared by
//! `set-password` and `bootstrap-household` lives in [`password_prompt`].
//!
//! Fixture mode vs live mode: the read-only commands [`validate`] and
//! [`optimise`] choose their data source from the **presence of `--db`**. With
//! no `--db`, they run in *fixture mode* — [`fixture_db::build_fixture_pool`]
//! builds a private in-memory SQLite database, applies only
//! `migrations/001_phase0_schema.sql`, and seeds it from the committed
//! `fixtures/` (no file is written). With `--db <path>`, they run in *live mode*
//! against that SQLite file, which is expected to have been populated by
//! `ingest-basketwatch` (and/or `ingest-lidl`) + `seed-matches`. The write
//! commands `ingest-basketwatch`, `ingest-lidl`, and `seed-matches` always
//! require a `--db` and never use fixture mode.

pub mod basket;
pub mod bootstrap;
pub mod embed;
pub mod fixture_db;
pub mod household;
pub mod ingest;
pub mod ingest_lidl;
pub mod no_network;
pub mod optimise;
pub mod password_prompt;
pub mod purchases;
pub mod receipts;
pub mod seed_matches;
pub mod set_password;
pub mod tesco;
pub mod validate;

use serde::Deserialize;

/// The fixed Phase-0 household. All seed data and fixtures share this id.
pub const PHASE0_HOUSEHOLD_ID: &str = "00000000-0000-0000-0000-000000000001";

/// A basket fixture (`fixtures/items.json`, `fixtures/weekly-shop.json`).
///
/// `items` are raw, free-text shopping-list lines (EN or ES); the count/size
/// parser and resolver turn them into priceable basket items at run time.
#[derive(Debug, Clone, Deserialize)]
pub struct BasketFixture {
    /// Free-text note documenting the fixture; ignored by the loader.
    #[serde(rename = "_note", default)]
    #[allow(dead_code)]
    pub note: String,
    /// Household the basket belongs to. Defaults to the Phase-0 household.
    #[serde(default = "default_household")]
    pub household_id: String,
    /// Raw shopping-list lines.
    pub items: Vec<String>,
}

fn default_household() -> String {
    PHASE0_HOUSEHOLD_ID.to_string()
}

/// Format a cents value as a `€X.XX` string (integer money; no float).
pub fn euros(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    format!("{sign}€{}.{:02}", abs / 100, abs % 100)
}
