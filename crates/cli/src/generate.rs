//! Deterministic seed-fixture generator.
//!
//! Reads `fixtures/dictionary.json` and writes `fixtures/canonical_items.json`
//! plus `migrations/002_seed.sql`. Both outputs are byte-stable across runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::model::{Category, Dictionary, GeneratedAlias, GeneratedFixture, GeneratedItem};
use crate::normalise::normalise;

/// Fixed dev household UUID (Phase 0 only; Phase 1 creates real households).
/// Authoritative value lives in `fixtures/dictionary.json`; this constant exists
/// for tests that assert the ID-derivation formula independently of the file.
#[cfg(test)]
const HOUSEHOLD_ID: &str = "00000000-0000-0000-0000-000000000001";

/// Fixed creation timestamp. NEVER `now()` — a constant keeps regeneration
/// byte-identical so the committed fixtures and migration never churn.
const CREATED_AT: &str = "2024-01-01T00:00:00Z";

/// Languages each variant is dual-inserted under. `language` is an INPUT-LOCALE
/// MATCHING BUCKET, not a linguistic assertion.
const LANGUAGES: [&str; 2] = ["en", "es"];

/// Lowercase hex SHA-256 of `input`.
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Deterministic canonical_items.id = hex(SHA-256("<household_id>:<slug>")).
fn item_id(household_id: &str, slug: &str) -> String {
    sha256_hex(&format!("{household_id}:{slug}"))
}

/// Deterministic item_aliases.id = hex(SHA-256("<item_id>:<language>:<alias>")).
fn alias_id(item_id: &str, language: &str, normalised_alias: &str) -> String {
    sha256_hex(&format!("{item_id}:{language}:{normalised_alias}"))
}

/// Build the in-memory generated fixture from the dictionary document.
///
/// Validates slugs (non-empty, unique) and category references, normalises all
/// aliases, dual-inserts variants under both languages, then dedupes aliases on
/// (household_id, lower(alias), language) so the unique index can never be hit.
pub fn build_fixture(dict: &Dictionary) -> Result<GeneratedFixture> {
    // Category key -> uuid lookup.
    let mut cat_by_key = std::collections::HashMap::new();
    for c in &dict.categories {
        cat_by_key.insert(c.key.as_str(), c.uuid.as_str());
    }

    let mut seen_slugs: HashSet<&str> = HashSet::new();
    let mut items: Vec<GeneratedItem> = Vec::with_capacity(dict.items.len());

    for entry in &dict.items {
        if entry.slug.trim().is_empty() {
            bail!(
                "dictionary entry has empty slug (name_en={:?})",
                entry.name_en
            );
        }
        if !seen_slugs.insert(entry.slug.as_str()) {
            bail!("duplicate slug in dictionary: {:?}", entry.slug);
        }

        let category_id = cat_by_key
            .get(entry.category.as_str())
            .copied()
            .with_context(|| {
                format!(
                    "item {:?} references unknown category {:?}",
                    entry.slug, entry.category
                )
            })?;

        let id = item_id(&dict.household_id, &entry.slug);

        // Gather candidate aliases: names + every variant under both languages.
        // De-dupe on (lower(alias), language) per the unique index.
        let mut seen_alias: HashSet<(String, &str)> = HashSet::new();
        let mut aliases: Vec<GeneratedAlias> = Vec::new();

        let push_alias = |raw: &str,
                          language: &'static str,
                          aliases: &mut Vec<GeneratedAlias>,
                          seen: &mut HashSet<(String, &'static str)>| {
            let alias = normalise(raw);
            if alias.is_empty() {
                return;
            }
            let key = (alias.to_lowercase(), language);
            if !seen.insert(key) {
                return;
            }
            aliases.push(GeneratedAlias {
                id: alias_id(&id, language, &alias),
                alias,
                language: language.to_string(),
                source: "manual".to_string(),
            });
        };

        // name_en -> 'en', name_es -> 'es'.
        push_alias(&entry.name_en, "en", &mut aliases, &mut seen_alias);
        push_alias(&entry.name_es, "es", &mut aliases, &mut seen_alias);

        // Each variant under BOTH languages (input-locale matching buckets).
        for variant in &entry.variants {
            for &language in &LANGUAGES {
                push_alias(variant, language, &mut aliases, &mut seen_alias);
            }
        }

        // Deterministic alias ordering: (language, alias).
        aliases.sort_by(|a, b| {
            a.language
                .cmp(&b.language)
                .then_with(|| a.alias.cmp(&b.alias))
        });

        items.push(GeneratedItem {
            id,
            slug: entry.slug.clone(),
            name_en: entry.name_en.clone(),
            name_es: entry.name_es.clone(),
            category_key: entry.category.clone(),
            category_id: category_id.to_string(),
            default_constraint_policy: entry.default_constraint_policy.clone(),
            provenance: entry.provenance.clone(),
            aliases,
        });
    }

    // Deterministic item ordering: by slug.
    items.sort_by(|a, b| a.slug.cmp(&b.slug));

    Ok(GeneratedFixture {
        note: GENERATED_FIXTURE_NOTE.to_string(),
        household_id: dict.household_id.clone(),
        created_at: CREATED_AT.to_string(),
        canonical_items: items,
    })
}

const GENERATED_FIXTURE_NOTE: &str = "GENERATED by `buywise-cli generate-fixtures` from fixtures/dictionary.json. \
Do not edit by hand. item_aliases.language is an INPUT-LOCALE MATCHING BUCKET, not a linguistic assertion: \
each variant is inserted under both 'en' and 'es'. Future matching MUST treat language as a ranking HINT \
(not a hard filter) and GROUP BY canonical_item_id so dual-inserted aliases don't double-count as match votes.";

/// Pretty-print the generated fixture as JSON (trailing newline for stable diffs).
pub fn render_canonical_items_json(fixture: &GeneratedFixture) -> Result<String> {
    let mut json = serde_json::to_string_pretty(fixture)?;
    json.push('\n');
    Ok(json)
}

/// SQL-escape a string literal (double single-quotes).
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Render `migrations/002_seed.sql` deterministically.
///
/// Categories emitted by `default_order`; canonical_items by slug; aliases by
/// (slug, language, alias).
pub fn render_seed_sql(dict: &Dictionary, fixture: &GeneratedFixture) -> String {
    let mut out = String::new();
    out.push_str(SEED_SQL_HEADER);

    // -- categories (ordered by default_order) --
    let mut categories: Vec<&Category> = dict.categories.iter().collect();
    categories.sort_by_key(|c| c.default_order);

    out.push_str("\nINSERT INTO categories (id, name_en, name_es, icon, default_order) VALUES\n");
    for (i, c) in categories.iter().enumerate() {
        let sep = if i + 1 == categories.len() { ";" } else { "," };
        out.push_str(&format!(
            "  ({}, {}, {}, {}, {}){}\n",
            sql_str(&c.uuid),
            sql_str(&c.name_en),
            sql_str(&c.name_es),
            sql_str(&c.icon),
            c.default_order,
            sep,
        ));
    }

    // -- canonical_items (ordered by slug; fixture is already slug-sorted) --
    out.push_str(
        "\nINSERT INTO canonical_items \
         (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) VALUES\n",
    );
    for (i, item) in fixture.canonical_items.iter().enumerate() {
        let sep = if i + 1 == fixture.canonical_items.len() {
            ";"
        } else {
            ","
        };
        out.push_str(&format!(
            "  ({}, {}, {}, {}, {}, {}, {}){}\n",
            sql_str(&item.id),
            sql_str(&fixture.household_id),
            sql_str(&item.name_en),
            sql_str(&item.name_es),
            sql_str(&item.category_id),
            sql_str(&item.default_constraint_policy),
            sql_str(&fixture.created_at),
            sep,
        ));
    }

    // -- item_aliases (ordered by slug, then language, then alias) --
    // Aliases within each item are already (language, alias)-sorted; items are
    // slug-sorted; so emitting in iteration order is (slug, language, alias).
    let total_aliases: usize = fixture
        .canonical_items
        .iter()
        .map(|i| i.aliases.len())
        .sum();
    out.push_str(
        "\nINSERT INTO item_aliases \
         (id, household_id, canonical_item_id, alias, language, source, created_at) VALUES\n",
    );
    let mut emitted = 0usize;
    for item in &fixture.canonical_items {
        for alias in &item.aliases {
            emitted += 1;
            let sep = if emitted == total_aliases { ";" } else { "," };
            out.push_str(&format!(
                "  ({}, {}, {}, {}, {}, {}, {}){}\n",
                sql_str(&alias.id),
                sql_str(&fixture.household_id),
                sql_str(&item.id),
                sql_str(&alias.alias),
                sql_str(&alias.language),
                sql_str(&alias.source),
                sql_str(&fixture.created_at),
                sep,
            ));
        }
    }

    out
}

const SEED_SQL_HEADER: &str = "\
-- 002_seed.sql
--
-- GENERATED by `cargo run -p buywise-cli -- generate-fixtures` from
-- fixtures/dictionary.json (the single source of truth, ported from Shoppy).
-- DO NOT EDIT BY HAND. Edit fixtures/dictionary.json and regenerate.
--
-- Determinism: canonical_items.id = hex(SHA-256(\"<household_id>:<slug>\")),
-- item_aliases.id = hex(SHA-256(\"<canonical_item_id>:<language>:<normalised_alias>\")),
-- created_at is a fixed constant (never now()), and rows are emitted in a stable
-- order (categories by default_order, items by slug, aliases by language+alias).
-- Re-running the generator yields a byte-identical file.
--
-- LANGUAGE SEMANTICS: item_aliases.language is an INPUT-LOCALE MATCHING BUCKET,
-- not a linguistic assertion. Each variant is inserted under BOTH 'en' and 'es'.
-- The future resolver/matching MUST treat language as a ranking HINT (not a hard
-- filter) and GROUP BY canonical_item_id so dual-inserted variant aliases do not
-- double-count as match votes. source='manual' for all seeded aliases (the 001
-- CHECK allows manual|accepted_match|llm and is not altered here); real
-- provenance lives in fixtures/dictionary.json.
";

/// Resolve the workspace root from this crate's manifest dir (../../ from crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// Run the generator against the repo's default paths.
///
/// Reads `<root>/fixtures/dictionary.json`; writes
/// `<root>/fixtures/canonical_items.json` and `<root>/migrations/002_seed.sql`.
pub fn run() -> Result<()> {
    let root = workspace_root();
    let dict_path = root.join("fixtures").join("dictionary.json");
    let items_out = root.join("fixtures").join("canonical_items.json");
    let sql_out = root.join("migrations").join("002_seed.sql");
    generate(&dict_path, &items_out, &sql_out)
}

/// Core generation entry point with explicit paths (used by tests).
pub fn generate(dict_path: &Path, items_out: &Path, sql_out: &Path) -> Result<()> {
    let dict = load_dictionary(dict_path)?;
    let fixture = build_fixture(&dict)?;

    let items_json = render_canonical_items_json(&fixture)?;
    let seed_sql = render_seed_sql(&dict, &fixture);

    std::fs::write(items_out, items_json)
        .with_context(|| format!("writing {}", items_out.display()))?;
    std::fs::write(sql_out, seed_sql).with_context(|| format!("writing {}", sql_out.display()))?;

    let alias_count: usize = fixture
        .canonical_items
        .iter()
        .map(|i| i.aliases.len())
        .sum();
    println!(
        "generated {} categories, {} canonical_items, {} aliases\n  -> {}\n  -> {}",
        dict.categories.len(),
        fixture.canonical_items.len(),
        alias_count,
        items_out.display(),
        sql_out.display(),
    );
    Ok(())
}

/// Load and parse `fixtures/dictionary.json`.
pub fn load_dictionary(path: &Path) -> Result<Dictionary> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let dict: Dictionary =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(dict)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        workspace_root()
    }

    fn load() -> Dictionary {
        load_dictionary(&root().join("fixtures").join("dictionary.json")).expect("load dict")
    }

    #[test]
    fn fixture_has_unique_item_ids_and_no_alias_dups() {
        let dict = load();
        let fixture = build_fixture(&dict).expect("build");

        let mut ids = HashSet::new();
        for item in &fixture.canonical_items {
            assert!(ids.insert(item.id.clone()), "duplicate item id {}", item.id);
        }

        // (household, lower(alias), language) must be globally unique.
        let mut keys = HashSet::new();
        for item in &fixture.canonical_items {
            for a in &item.aliases {
                let key = (a.alias.to_lowercase(), a.language.clone());
                assert!(keys.insert(key.clone()), "duplicate alias key {key:?}");
            }
        }
    }

    #[test]
    fn milk_has_leche_alias_under_es() {
        let dict = load();
        let fixture = build_fixture(&dict).expect("build");
        let milk = fixture
            .canonical_items
            .iter()
            .find(|i| i.slug == "milk")
            .expect("milk item");
        assert_eq!(milk.name_en, "Milk");
        assert!(milk
            .aliases
            .iter()
            .any(|a| a.alias == "leche" && a.language == "es"));
    }

    #[test]
    fn all_aliases_are_normalised() {
        let dict = load();
        let fixture = build_fixture(&dict).expect("build");
        for item in &fixture.canonical_items {
            for a in &item.aliases {
                assert_eq!(
                    a.alias,
                    normalise(&a.alias),
                    "alias not normalised: {}",
                    a.alias
                );
            }
        }
    }

    #[test]
    fn deterministic_ids_are_stable() {
        // Recomputing the ID for a known slug must match the hash formula.
        let expected = sha256_hex(&format!("{HOUSEHOLD_ID}:milk"));
        assert_eq!(item_id(HOUSEHOLD_ID, "milk"), expected);
    }
}
