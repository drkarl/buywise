//! Local serde models for the seed generator.
//!
//! These are intentionally self-contained: the generator must stay independent
//! of `crates/core`'s domain types (P0-T4, not yet implemented). They mirror only
//! what `fixtures/dictionary.json` carries and what `fixtures/canonical_items.json`
//! emits — nothing more.

use serde::{Deserialize, Serialize};

fn default_policy() -> String {
    "any".to_string()
}

/// Top-level `fixtures/dictionary.json` document (the single source of truth).
#[derive(Debug, Clone, Deserialize)]
pub struct Dictionary {
    /// Free-text note (ignored by the generator; documents the file's contract).
    #[serde(rename = "_note", default)]
    pub note: String,
    pub household_id: String,
    pub categories: Vec<Category>,
    pub items: Vec<DictionaryItem>,
}

/// A grocery category (11 of them), ported from Shoppy's fixed UUIDs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Category {
    /// Stable machine key derived from the category UUID (e.g. "produce").
    pub key: String,
    pub uuid: String,
    pub name_en: String,
    pub name_es: String,
    pub icon: String,
    pub default_order: i64,
}

/// A dictionary entry — one canonical grocery item plus its raw variant strings.
#[derive(Debug, Clone, Deserialize)]
pub struct DictionaryItem {
    /// Permanent immutable identifier; drives the canonical_items.id hash.
    pub slug: String,
    pub name_en: String,
    pub name_es: String,
    /// Category key (matches `Category::key`).
    pub category: String,
    #[serde(default = "default_policy")]
    pub default_constraint_policy: String,
    #[serde(default)]
    pub variants: Vec<String>,
    #[serde(default)]
    pub provenance: String,
}

/// Top-level `fixtures/canonical_items.json` document (generated output).
#[derive(Debug, Clone, Serialize)]
pub struct GeneratedFixture {
    #[serde(rename = "_note")]
    pub note: String,
    pub household_id: String,
    pub created_at: String,
    pub canonical_items: Vec<GeneratedItem>,
}

/// A generated canonical item with its computed ID and alias rows.
#[derive(Debug, Clone, Serialize)]
pub struct GeneratedItem {
    pub id: String,
    pub slug: String,
    pub name_en: String,
    pub name_es: String,
    pub category_key: String,
    pub category_id: String,
    pub default_constraint_policy: String,
    pub provenance: String,
    pub aliases: Vec<GeneratedAlias>,
}

/// A generated alias row (deduped on household_id + lower(alias) + language).
#[derive(Debug, Clone, Serialize)]
pub struct GeneratedAlias {
    pub id: String,
    pub alias: String,
    pub language: String,
    pub source: String,
}
