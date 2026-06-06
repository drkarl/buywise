//! Phase-0 bilingual resolution cascade (alias lookup + token/fuzzy match).
//!
//! Resolves a raw shopping-list term (entered in English or Spanish) to a
//! `canonical_items.id`. Implements steps 1 and 2 of the resolution cascade
//! described in `docs/design.md` §8:
//!
//! 1. **Alias lookup** — exact match on the *normalised* alias in
//!    `item_aliases` ([`alias_lookup`]).
//! 2. **Token / fuzzy match** — approximate matching against every canonical
//!    item's `name_en`, `name_es`, and aliases ([`fuzzy_match`]).
//!
//! Steps 3 (LLM fallback) and 4 (user clarification) are Phase-3 work and are
//! deliberately omitted: [`resolve`] returns the step-2 outcome unchanged.
//!
//! All comparison happens on text normalised by [`crate::normalise::normalize`]
//! (lowercased, accent-stripped, punctuation→space). The seed stores aliases in
//! that same normalised form, so a `lower(alias) = normalize(input)` comparison
//! is an exact normalised match.
//!
//! ## Bilingual seed
//!
//! `item_aliases.language` is an input-locale *matching bucket*, not a
//! linguistic assertion: the seed may store the same item's variants under both
//! `'en'` and `'es'` (it does so for some terms but NOT all — e.g. "leche" and
//! "jamon" are seeded `'es'`-only; see `migrations/002_seed.sql`). Language is
//! therefore treated as a ranking hint only — never a filter — and every query
//! groups/`DISTINCT`s by `canonical_item_id` so an item that does have rows in
//! both buckets still counts once.
//!
//! ## Thresholds (ported from Shoppy `src/lib/matching/fuzzy.ts`)
//!
//! * [`FUZZY_DISTANCE_THRESHOLD`] = 2 — max Levenshtein distance.
//! * [`MIN_FUZZY_LENGTH`] = 4 — both strings must be at least this long to be
//!   compared by edit distance.
//! * [`MIN_CONTAINS_LENGTH`] = 5 — the shorter string must be at least this long
//!   for a bidirectional substring (`contains`) match.

use std::collections::BTreeSet;

use sqlx::sqlite::{SqliteConnection, SqlitePool};
use strsim::levenshtein;

use crate::db::DbError;
use crate::normalise::normalize;

/// Maximum Levenshtein distance for a fuzzy (edit-distance) match.
pub const FUZZY_DISTANCE_THRESHOLD: usize = 2;
/// Minimum length (both strings) required before an edit-distance comparison.
pub const MIN_FUZZY_LENGTH: usize = 4;
/// Minimum length of the shorter string for a bidirectional `contains` match.
pub const MIN_CONTAINS_LENGTH: usize = 5;

/// The outcome of resolving a raw term against the household catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionOutcome {
    /// A single unambiguous canonical item.
    SingleMatch { canonical_item_id: String },
    /// Several plausible canonical items — requires user selection (Phase 3).
    /// Candidate ids are de-duplicated and sorted for deterministic results.
    MultipleCandidates { candidates: Vec<String> },
    /// No match via alias or fuzzy — requires LLM or user (Phase 3).
    NoMatch,
}

/// Step 1: exact alias lookup on the normalised alias.
///
/// Returns the `canonical_item_id` whose normalised alias equals
/// `normalize(alias)`, or `None` if there is no match.
///
/// The query selects `DISTINCT canonical_item_id`, so when one item DOES have the
/// same normalised alias under both `'en'` and `'es'`, those rows collapse to a
/// single id and are never double-counted. Language is not used as a filter.
///
/// If MORE THAN ONE distinct canonical item shares the exact normalised alias
/// (possible across languages, since the seed's collision guard was
/// per-language), this returns `None` so the cascade falls through to
/// [`fuzzy_match`], which will surface the ambiguity as
/// [`ResolutionOutcome::MultipleCandidates`]. This keeps step 1 strictly a
/// *single, unambiguous* match.
pub async fn alias_lookup(
    alias: &str,
    household_id: &str,
    db: &SqlitePool,
) -> Result<Option<String>, DbError> {
    let normalised = normalize(alias);
    if normalised.is_empty() {
        return Ok(None);
    }

    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT canonical_item_id \
         FROM item_aliases \
         WHERE household_id = ?1 AND lower(alias) = ?2",
    )
    .bind(household_id)
    .bind(&normalised)
    .fetch_all(db)
    .await?;

    match ids.len() {
        1 => Ok(Some(ids.into_iter().next().expect("len checked"))),
        // Zero matches, or an ambiguous >1 distinct-item collision: defer to
        // fuzzy matching rather than guessing.
        _ => Ok(None),
    }
}

/// One catalogue string (a name or an alias) tagged with its canonical item.
struct CandidateString {
    canonical_item_id: String,
    /// Already normalised via [`normalize`].
    text: String,
}

/// Step 2: token/fuzzy match against `name_en`, `name_es`, and all aliases.
///
/// Builds the candidate corpus once (two queries: canonical names + aliases),
/// normalises both sides, and marks a canonical item as a candidate if ANY of
/// its strings matches `text` by one of three rules (see [`is_match`]):
/// exact normalised equality, Levenshtein distance ≤ [`FUZZY_DISTANCE_THRESHOLD`]
/// (both strings ≥ [`MIN_FUZZY_LENGTH`]), or bidirectional `contains` (shorter
/// string ≥ [`MIN_CONTAINS_LENGTH`]).
///
/// Matching ids are collected into a [`BTreeSet`], so an item matched via
/// several of its strings (or via both languages) counts once and the result
/// is in a stable, sorted order. The count of distinct ids decides the outcome:
/// 1 → [`ResolutionOutcome::SingleMatch`], ≥2 →
/// [`ResolutionOutcome::MultipleCandidates`], 0 → [`ResolutionOutcome::NoMatch`].
pub async fn fuzzy_match(
    text: &str,
    household_id: &str,
    db: &SqlitePool,
) -> Result<ResolutionOutcome, DbError> {
    let needle = normalize(text);
    if needle.is_empty() {
        return Ok(ResolutionOutcome::NoMatch);
    }

    let corpus = load_corpus(household_id, db).await?;

    let mut matched: BTreeSet<String> = BTreeSet::new();
    for candidate in &corpus {
        if is_match(&needle, &candidate.text) {
            matched.insert(candidate.canonical_item_id.clone());
        }
    }

    let candidates: Vec<String> = matched.into_iter().collect();
    Ok(match candidates.len() {
        0 => ResolutionOutcome::NoMatch,
        1 => ResolutionOutcome::SingleMatch {
            canonical_item_id: candidates.into_iter().next().expect("len checked"),
        },
        _ => ResolutionOutcome::MultipleCandidates { candidates },
    })
}

/// Load every comparable string for the household: canonical `name_en` /
/// `name_es` plus all aliases, each tagged with its `canonical_item_id` and
/// normalised. Two queries (rather than per-row SQL); the fuzzy comparison runs
/// in Rust over the ~hundreds of seeded items.
async fn load_corpus(household_id: &str, db: &SqlitePool) -> Result<Vec<CandidateString>, DbError> {
    let names: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, name_en, name_es FROM canonical_items WHERE household_id = ?1")
            .bind(household_id)
            .fetch_all(db)
            .await?;

    let aliases: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT canonical_item_id, alias FROM item_aliases WHERE household_id = ?1",
    )
    .bind(household_id)
    .fetch_all(db)
    .await?;

    let mut corpus = Vec::with_capacity(names.len() * 2 + aliases.len());
    for (id, name_en, name_es) in names {
        corpus.push(CandidateString {
            canonical_item_id: id.clone(),
            text: normalize(&name_en),
        });
        corpus.push(CandidateString {
            canonical_item_id: id,
            text: normalize(&name_es),
        });
    }
    for (id, alias) in aliases {
        corpus.push(CandidateString {
            canonical_item_id: id,
            text: normalize(&alias),
        });
    }
    Ok(corpus)
}

/// Whether two already-normalised strings match under the Phase-0 rules:
/// exact equality, Levenshtein distance ≤ [`FUZZY_DISTANCE_THRESHOLD`] when both
/// are ≥ [`MIN_FUZZY_LENGTH`], or bidirectional `contains` when the shorter is
/// ≥ [`MIN_CONTAINS_LENGTH`].
fn is_match(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if is_fuzzy_similar(a, b) {
        return true;
    }
    contains_bidirectional(a, b)
}

/// Edit-distance rule: both strings at least [`MIN_FUZZY_LENGTH`] long and within
/// [`FUZZY_DISTANCE_THRESHOLD`] Levenshtein edits. Lengths are measured in
/// Unicode scalar values to stay consistent with [`levenshtein`], which also
/// operates on `char`s.
fn is_fuzzy_similar(a: &str, b: &str) -> bool {
    let (len_a, len_b) = (a.chars().count(), b.chars().count());
    if len_a < MIN_FUZZY_LENGTH || len_b < MIN_FUZZY_LENGTH {
        return false;
    }
    // A length difference greater than the threshold cannot fall within it
    // (the distance is at least the length difference); cheap early-out.
    if len_a.abs_diff(len_b) > FUZZY_DISTANCE_THRESHOLD {
        return false;
    }
    levenshtein(a, b) <= FUZZY_DISTANCE_THRESHOLD
}

/// Bidirectional `contains` rule: one string contains the other, and the
/// SHORTER (contained) string is at least [`MIN_CONTAINS_LENGTH`] long.
fn contains_bidirectional(a: &str, b: &str) -> bool {
    let shorter_len = a.chars().count().min(b.chars().count());
    if shorter_len < MIN_CONTAINS_LENGTH {
        return false;
    }
    a.contains(b) || b.contains(a)
}

/// Full Phase-0 resolution cascade.
///
/// Normalises `raw_text`, tries [`alias_lookup`] first (so an exact alias always
/// wins over any fuzzy candidate), and on `None` falls through to
/// [`fuzzy_match`], returning its outcome directly. The LLM (step 3) and user
/// clarification (step 4) stages are Phase 3 and are not invoked here.
pub async fn resolve(
    raw_text: &str,
    household_id: &str,
    db: &SqlitePool,
) -> Result<ResolutionOutcome, DbError> {
    if let Some(canonical_item_id) = alias_lookup(raw_text, household_id, db).await? {
        return Ok(ResolutionOutcome::SingleMatch { canonical_item_id });
    }
    fuzzy_match(raw_text, household_id, db).await
}

/// The `source` of a learned alias, mirroring the `item_aliases.source` CHECK
/// (`manual` | `accepted_match` | `llm`). Step 1 of the cascade never learns an
/// alias (it already exists), so only the two automatic / confirmed sources that
/// the feedback loop writes are modelled here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasSource {
    /// A mapping confirmed by a fuzzy single-match (step 2) or a user
    /// confirmation (step 4) — both persist as `source='accepted_match'`.
    AcceptedMatch,
    /// A mapping discovered by the LLM fallback (step 3) — `source='llm'`.
    Llm,
}

impl AliasSource {
    /// The literal `item_aliases.source` text for this source.
    fn as_str(self) -> &'static str {
        match self {
            AliasSource::AcceptedMatch => "accepted_match",
            AliasSource::Llm => "llm",
        }
    }
}

/// Infer the input-locale matching bucket (`'en'` | `'es'`) for a learned alias.
///
/// `item_aliases.language` is an input-locale bucket, not a linguistic assertion
/// (see the module docs and `migrations/002_seed.sql`). We bucket a learned term
/// as `'es'` when it matches the canonical item's Spanish face — its `name_es` or
/// any of its existing `'es'`-bucket aliases — by normalised equality, and as
/// `'en'` otherwise. This keeps the default `'en'` for an English term or any
/// term we cannot confidently place (the design's "default `en` if unclear"),
/// while a clearly-Spanish confirmation (e.g. "leche") lands in the `'es'`
/// bucket so a later Spanish add hits step 1 directly.
///
/// `normalised` is the already-normalised alias; `canonical_item_id` is the item
/// it is being learned for. Runs on the caller's connection/transaction.
///
/// NOTE: the unique index key includes `language`, so the bucketing chosen here
/// interacts with dedup. If the same normalised term is learned for item A as
/// `'en'` and later for item B (whose `name_es` matches) as `'es'`, BOTH rows
/// insert — and [`alias_lookup`]'s `DISTINCT canonical_item_id` then sees two ids
/// and returns `None`, disabling the step-1 short-circuit for that term. Inherent
/// to the pre-existing per-language index; an unlikely corner, noted so a future
/// reader knows the two interact.
async fn infer_alias_language(
    conn: &mut SqliteConnection,
    canonical_item_id: &str,
    normalised: &str,
) -> Result<&'static str, DbError> {
    // The item's Spanish catalogue name. A normalised match → Spanish bucket.
    let name_es: Option<String> =
        sqlx::query_scalar("SELECT name_es FROM canonical_items WHERE id = ?1")
            .bind(canonical_item_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(name_es) = name_es {
        if normalize(&name_es) == normalised {
            return Ok("es");
        }
    }

    // Any existing `'es'`-bucket alias for the item whose normalised form equals
    // this term also places it in the Spanish bucket. (The seed buckets Spanish
    // variants under `'es'` — e.g. "leche"/"jamon" are `'es'`-only — so a Spanish
    // term that was seeded typically has an `'es'` row to match here. The seed does
    // NOT dual-insert every variant under both buckets; see migrations/002_seed.sql.)
    let es_alias_hit: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM item_aliases \
         WHERE canonical_item_id = ?1 AND language = 'es' AND lower(alias) = ?2 \
         LIMIT 1",
    )
    .bind(canonical_item_id)
    .bind(normalised)
    .fetch_optional(&mut *conn)
    .await?;
    if es_alias_hit.is_some() {
        return Ok("es");
    }

    // English term, or one we cannot confidently place: default to `'en'`.
    Ok("en")
}

/// Persist a confirmed/learned `raw_text` → canonical-item mapping into
/// `item_aliases`, the single write path for the resolution feedback loop
/// (P3-T4). Steps 2 (fuzzy single-match), 3 (LLM), and 4 (user confirmation) all
/// funnel through here so the `source`, normalisation, language inference, and
/// dedup semantics are identical across paths.
///
/// Behaviour:
///   * The alias is stored NORMALISED via [`normalize`] (the same form step-1
///     [`alias_lookup`] compares against), so the next add of the same term is an
///     exact normalised hit and resolves via step 1 with no fuzzy/LLM work. A
///     term that normalises to empty learns nothing and is a no-op.
///   * `language` is inferred by [`infer_alias_language`] (`'es'` for a Spanish
///     term, else `'en'` — the design's "default `en` if unclear").
///   * `source` is the supplied [`AliasSource`] (`accepted_match` or `llm`).
///   * `INSERT OR IGNORE` makes the write idempotent against the unique index
///     `uq_item_aliases_household_alias_lang` on
///     `(household_id, lower(alias), language)`: confirming the same term twice
///     is a harmless no-op rather than a constraint error.
///
/// Runs on the caller's connection (a `&mut SqliteConnection`, reachable from a
/// transaction via `&mut **tx`) so the alias commits atomically with the guarded
/// bind/merge UPDATE that proved the entry's `raw_text` was still current — the
/// mapping is written iff the bind it justifies also commits, never on a stale
/// no-op.
pub async fn commit_alias(
    conn: &mut SqliteConnection,
    household_id: &str,
    canonical_item_id: &str,
    raw_text: &str,
    source: AliasSource,
) -> Result<(), DbError> {
    let normalised = normalize(raw_text);
    if normalised.is_empty() {
        return Ok(());
    }
    let language = infer_alias_language(conn, canonical_item_id, &normalised).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::db::now_iso();
    sqlx::query(
        "INSERT OR IGNORE INTO item_aliases \
             (id, household_id, canonical_item_id, alias, language, source, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(&id)
    .bind(household_id)
    .bind(canonical_item_id)
    .bind(&normalised)
    .bind(language)
    .bind(source.as_str())
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect, run_migrations};

    const HOUSEHOLD: &str = "00000000-0000-0000-0000-000000000001";

    // Canonical ids from migrations/002_seed.sql (deterministic SHA-256 slugs).
    const MILK_ID: &str = "2c41ac7cde2aad18bbf43e93541efe13da56585f0e8965ed93a5b6e60d6b01e8";
    const HAM_ID: &str = "81d5cf4dc7c8081157fe986111c79ad49427cbca7a287a18dc1f3955bdfb1644";

    async fn pool() -> SqlitePool {
        let pool = connect("sqlite::memory:").await.expect("connect in-memory");
        run_migrations(&pool).await.expect("run migrations");
        pool
    }

    /// Look up name_en for a canonical id, to assert an id maps to the expected
    /// item without hard-coding the id in every test.
    async fn name_en(pool: &SqlitePool, id: &str) -> String {
        sqlx::query_scalar("SELECT name_en FROM canonical_items WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("canonical item exists")
    }

    /// Look up a canonical id by its exact `name_en`, so a test can assert a
    /// known item is present without hard-coding the SHA-256 slug.
    async fn id_by_name_en(pool: &SqlitePool, name_en: &str) -> String {
        sqlx::query_scalar("SELECT id FROM canonical_items WHERE name_en = ?1")
            .bind(name_en)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|_| panic!("canonical item {name_en:?} exists in seed"))
    }

    fn single(outcome: ResolutionOutcome) -> String {
        match outcome {
            ResolutionOutcome::SingleMatch { canonical_item_id } => canonical_item_id,
            other => panic!("expected SingleMatch, got {other:?}"),
        }
    }

    // ---- in-line unit rules (no DB) -----------------------------------------

    #[test]
    fn fuzzy_levenshtein_boundary() {
        // Distance ≤ 2 matches; 3 does not (both strings ≥ 4 long).
        assert!(is_fuzzy_similar("melon", "lemon")); // distance 2 (two substitutions)
        assert!(is_fuzzy_similar("milks", "milk")); // distance 1
        assert!(is_fuzzy_similar("tomato", "tomatos")); // distance 1
        assert!(!is_fuzzy_similar("milk", "water")); // distance 5
                                                     // Exact-distance pairs at the boundary (both ≥ 4 long).
        assert!(is_fuzzy_similar("abcde", "abxye")); // distance 2
        assert!(!is_fuzzy_similar("abcde", "axyze")); // distance 3
    }

    #[test]
    fn fuzzy_min_length_guard() {
        // Sub-minLength strings never fuzzy-match, even at distance 1.
        assert!(!is_fuzzy_similar("cat", "car")); // both len 3
        assert!(!is_fuzzy_similar("egg", "ego")); // len 3
                                                  // Length 4 is allowed.
        assert!(is_fuzzy_similar("milk", "milt")); // distance 1, both len 4
    }

    #[test]
    fn contains_min_length_guard() {
        // Shorter string ≥ 5 → contains match.
        assert!(contains_bidirectional("chocolate", "choco")); // "choco" len 5
        assert!(contains_bidirectional("choco", "chocolate")); // symmetric
                                                               // Shorter string len 4 → no contains match.
        assert!(!contains_bidirectional("chocolate", "late"));
        // "milk" (len 4) inside "soy milk" must NOT contains-match.
        assert!(!contains_bidirectional("milk", "soy milk"));
    }

    // ---- alias_lookup -------------------------------------------------------

    #[tokio::test]
    async fn alias_lookup_exact_bilingual() {
        let pool = pool().await;
        // "leche" (es alias) and "milk" (en alias) → same Milk id.
        let es = alias_lookup("leche", HOUSEHOLD, &pool).await.unwrap();
        let en = alias_lookup("milk", HOUSEHOLD, &pool).await.unwrap();
        assert_eq!(es.as_deref(), Some(MILK_ID));
        assert_eq!(en.as_deref(), Some(MILK_ID));
        assert_eq!(es, en, "leche and milk resolve to the same canonical id");
    }

    #[tokio::test]
    async fn alias_lookup_accent_and_case_insensitive() {
        let pool = pool().await;
        // Input "Jamón" normalises to "jamon"; the seeded alias is "jamon".
        let id = alias_lookup("Jamón", HOUSEHOLD, &pool).await.unwrap();
        assert_eq!(id.as_deref(), Some(HAM_ID));
        assert_eq!(name_en(&pool, HAM_ID).await, "Ham");
    }

    #[tokio::test]
    async fn alias_lookup_unknown_is_none() {
        let pool = pool().await;
        let id = alias_lookup("xyzqrst_unknown", HOUSEHOLD, &pool)
            .await
            .unwrap();
        assert_eq!(id, None);
    }

    #[tokio::test]
    async fn alias_lookup_wrong_household_is_none() {
        let pool = pool().await;
        let id = alias_lookup("leche", "ffffffff-0000-0000-0000-000000000000", &pool)
            .await
            .unwrap();
        assert_eq!(id, None, "aliases are scoped per household");
    }

    // ---- resolve: acceptance criteria ---------------------------------------

    #[tokio::test]
    async fn resolve_leche_is_milk() {
        let pool = pool().await;
        let id = single(resolve("leche", HOUSEHOLD, &pool).await.unwrap());
        assert_eq!(id, MILK_ID);
        assert_eq!(name_en(&pool, &id).await, "Milk");
    }

    #[tokio::test]
    async fn resolve_milk_same_id_as_leche() {
        let pool = pool().await;
        let leche = single(resolve("leche", HOUSEHOLD, &pool).await.unwrap());
        let milk = single(resolve("milk", HOUSEHOLD, &pool).await.unwrap());
        assert_eq!(
            milk, leche,
            "milk and leche resolve to the same canonical id"
        );
    }

    #[tokio::test]
    async fn resolve_jamon_is_ham() {
        let pool = pool().await;
        // "jamon" normalises to "jamon" and matches the accent-stripped alias.
        let id = single(resolve("jamon", HOUSEHOLD, &pool).await.unwrap());
        assert_eq!(id, HAM_ID);
        assert_eq!(name_en(&pool, &id).await, "Ham");
    }

    #[tokio::test]
    async fn resolve_cream_is_multiple_candidates() {
        let pool = pool().await;
        // No exact alias "cream"; fuzzy contains-matches every item whose name
        // contains "cream" (Cream cheese, Heavy cream, Ice cream, Sour cream,
        // Shaving cream, Diaper cream, ...).
        let outcome = resolve("cream", HOUSEHOLD, &pool).await.unwrap();
        match outcome {
            ResolutionOutcome::MultipleCandidates { candidates } => {
                assert!(
                    candidates.len() >= 2,
                    "expected ≥2 cream candidates, got {}",
                    candidates.len()
                );
                // Sorted + de-duplicated (BTreeSet → Vec).
                let mut sorted = candidates.clone();
                sorted.sort();
                assert_eq!(
                    candidates, sorted,
                    "candidates must be in stable sorted order"
                );
                let dedup: BTreeSet<&String> = candidates.iter().collect();
                assert_eq!(dedup.len(), candidates.len(), "candidates must be unique");
                // The candidate set must contain the concrete known cream items,
                // so an accidental over- or under-match is caught. Ids are looked
                // up by name to stay robust against seed-slug changes.
                let sour_cream = id_by_name_en(&pool, "Sour cream").await;
                let ice_cream = id_by_name_en(&pool, "Ice cream").await;
                assert!(
                    candidates.contains(&sour_cream),
                    "expected Sour cream ({sour_cream}) among candidates: {candidates:?}"
                );
                assert!(
                    candidates.contains(&ice_cream),
                    "expected Ice cream ({ice_cream}) among candidates: {candidates:?}"
                );
            }
            other => panic!("expected MultipleCandidates, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_unknown_is_no_match() {
        let pool = pool().await;
        let outcome = resolve("xyzqrst_unknown", HOUSEHOLD, &pool).await.unwrap();
        assert_eq!(outcome, ResolutionOutcome::NoMatch);
    }

    // ---- ordering: alias tried before fuzzy ---------------------------------

    #[tokio::test]
    async fn resolve_prefers_exact_alias_over_fuzzy() {
        let pool = pool().await;
        // "milk" is an exact alias (SingleMatch Milk). Fuzzy alone over the
        // corpus would not change this, but the assertion documents that step 1
        // short-circuits: the result is the alias's item, returned as a single
        // unambiguous match rather than any multi-candidate fuzzy spread.
        let outcome = resolve("milk", HOUSEHOLD, &pool).await.unwrap();
        assert_eq!(
            outcome,
            ResolutionOutcome::SingleMatch {
                canonical_item_id: MILK_ID.to_string()
            }
        );
    }

    // ---- de-duplication / no double counting --------------------------------

    #[tokio::test]
    async fn item_not_double_counted_across_strings() {
        let pool = pool().await;
        // "ham" matches Ham via name_en ("Ham") AND the 'en' alias "ham"; the
        // Spanish bucket ("Jamon"/"jamon") is a different string but the SAME
        // item. All of these collapse to one canonical id → SingleMatch, never
        // MultipleCandidates. (Resolve hits the exact alias first, so assert
        // fuzzy_match directly to exercise the de-dup path.)
        let outcome = fuzzy_match("ham", HOUSEHOLD, &pool).await.unwrap();
        assert_eq!(
            outcome,
            ResolutionOutcome::SingleMatch {
                canonical_item_id: HAM_ID.to_string()
            },
            "Ham matched via name + alias + both languages must count once"
        );
    }

    // ---- ported Shoppy cases (DuplicateDetector.test.ts) --------------------

    #[tokio::test]
    async fn ported_exact_match_milk() {
        // Shoppy: checkForDuplicate("milk", [Milk]) → exact. Here the exact
        // (normalised) alias "milk" resolves to the Milk item.
        let pool = pool().await;
        assert_eq!(
            single(resolve("milk", HOUSEHOLD, &pool).await.unwrap()),
            MILK_ID
        );
    }

    #[tokio::test]
    async fn ported_bilingual_dictionary_match() {
        // Shoppy: "leche" → Milk (dictionary), "milk" → leche (dictionary).
        // Here both directions resolve to the same canonical id.
        let pool = pool().await;
        let a = single(resolve("leche", HOUSEHOLD, &pool).await.unwrap());
        let b = single(resolve("milk", HOUSEHOLD, &pool).await.unwrap());
        assert_eq!(a, b);
        assert_eq!(a, MILK_ID);
    }

    #[tokio::test]
    async fn ported_fuzzy_milks_to_milk() {
        // Shoppy tested "milks" vs a one-item list [Milk] → fuzzy (distance 1).
        // Against the FULL seed catalogue "milks" is genuinely ambiguous: it is
        // distance 1 from "milk" (Milk) AND distance 2 from "pilas" (the
        // Batteries 'es' alias), so the cascade correctly surfaces both as
        // MultipleCandidates. The ported guarantee is that the fuzzy path still
        // finds Milk among the candidates.
        let pool = pool().await;
        let outcome = fuzzy_match("milks", HOUSEHOLD, &pool).await.unwrap();
        match outcome {
            ResolutionOutcome::MultipleCandidates { candidates } => {
                assert!(
                    candidates.iter().any(|id| id == MILK_ID),
                    "milks must fuzzy-match Milk; got {candidates:?}"
                );
            }
            other => panic!("expected MultipleCandidates including Milk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ported_accent_removed_jamon() {
        // Shoppy: "jamon" matches after accent removal. Punctuation→space
        // variant of the BuyWise normaliser still resolves "Jamón" → Ham.
        let pool = pool().await;
        assert_eq!(
            single(resolve("Jamón", HOUSEHOLD, &pool).await.unwrap()),
            HAM_ID
        );
    }

    // ---- commit_alias: the P3-T4 feedback-loop write path -------------------

    /// Fetch the `(language, source)` of the alias for `normalised_term` pointing
    /// at `canonical_item_id`, scoped to the seed household. Panics if absent.
    async fn alias_lang_source(
        pool: &SqlitePool,
        canonical_item_id: &str,
        normalised_term: &str,
    ) -> (String, String) {
        sqlx::query_as(
            "SELECT language, source FROM item_aliases \
             WHERE household_id = ?1 AND canonical_item_id = ?2 AND lower(alias) = ?3",
        )
        .bind(HOUSEHOLD)
        .bind(canonical_item_id)
        .bind(normalised_term)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|_| panic!("alias {normalised_term:?} → {canonical_item_id} exists"))
    }

    async fn commit_via_conn(
        pool: &SqlitePool,
        canonical_item_id: &str,
        raw_text: &str,
        source: AliasSource,
    ) {
        let mut conn = pool.acquire().await.expect("acquire");
        commit_alias(&mut conn, HOUSEHOLD, canonical_item_id, raw_text, source)
            .await
            .expect("commit_alias");
    }

    #[tokio::test]
    async fn commit_alias_persists_source_and_normalises() {
        let pool = pool().await;
        // A novel English term for Milk, learned as an accepted_match. It is stored
        // NORMALISED ("My  Special-Milk!" → "my special milk") and resolves via the
        // step-1 alias lookup on the next add (the feedback-loop guarantee).
        commit_via_conn(
            &pool,
            MILK_ID,
            "My  Special-Milk!",
            AliasSource::AcceptedMatch,
        )
        .await;
        let (language, source) = alias_lang_source(&pool, MILK_ID, "my special milk").await;
        assert_eq!(source, "accepted_match");
        assert_eq!(
            language, "en",
            "an unknown English-looking term defaults to en"
        );

        // The normalised term now resolves at step 1, no fuzzy needed.
        assert_eq!(
            alias_lookup("MY special   milk", HOUSEHOLD, &pool)
                .await
                .unwrap()
                .as_deref(),
            Some(MILK_ID)
        );
    }

    #[tokio::test]
    async fn commit_alias_infers_spanish_from_name_es() {
        let pool = pool().await;
        // A fresh canonical item with a Spanish name that is NOT already a seeded
        // alias, so the only thing that can bucket a matching term as 'es' is the
        // `name_es` inference path (not a pre-existing seed row). Committing a term
        // equal to its name_es must land in the 'es' bucket with our source.
        let new_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        sqlx::query(
            "INSERT INTO canonical_items \
                 (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
             VALUES (?1, ?2, 'Quince paste', 'Dulce de membrillo', \
                     'c1000000-0000-0000-0000-000000000005', 'any', '2024-01-01T00:00:00Z')",
        )
        .bind(new_id)
        .bind(HOUSEHOLD)
        .execute(&pool)
        .await
        .expect("insert canonical item");

        commit_via_conn(&pool, new_id, "Dulce de Membrillo", AliasSource::Llm).await;
        let (language, source) = alias_lang_source(&pool, new_id, "dulce de membrillo").await;
        assert_eq!(language, "es", "a term equal to name_es buckets as Spanish");
        assert_eq!(source, "llm", "inference is independent of the source");
    }

    #[tokio::test]
    async fn commit_alias_infers_english_from_name_en() {
        let pool = pool().await;
        // A fresh canonical item: a term equal to its English name_en (and not its
        // Spanish name_es) buckets as 'en'. Self-contained so it tests the inference
        // path rather than reading back a pre-seeded row.
        let new_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        sqlx::query(
            "INSERT INTO canonical_items \
                 (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at) \
             VALUES (?1, ?2, 'Marshmallow fluff', 'Crema de malvavisco', \
                     'c1000000-0000-0000-0000-000000000005', 'any', '2024-01-01T00:00:00Z')",
        )
        .bind(new_id)
        .bind(HOUSEHOLD)
        .execute(&pool)
        .await
        .expect("insert canonical item");

        commit_via_conn(
            &pool,
            new_id,
            "Marshmallow Fluff",
            AliasSource::AcceptedMatch,
        )
        .await;
        let (language, source) = alias_lang_source(&pool, new_id, "marshmallow fluff").await;
        assert_eq!(language, "en");
        assert_eq!(source, "accepted_match");
    }

    #[tokio::test]
    async fn commit_alias_infers_spanish_from_existing_es_alias() {
        let pool = pool().await;
        // "jamon" is a seeded 'es'-bucket alias for Ham (its name_es is "Jamón",
        // which also normalises to "jamon"). Re-committing it must reuse the 'es'
        // bucket rather than inserting a fresh 'en' duplicate.
        commit_via_conn(&pool, HAM_ID, "Jamón", AliasSource::AcceptedMatch).await;
        let (language, _) = alias_lang_source(&pool, HAM_ID, "jamon").await;
        assert_eq!(language, "es");
    }

    #[tokio::test]
    async fn commit_alias_dedups_via_unique_index() {
        let pool = pool().await;
        let before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases WHERE household_id = ?1")
                .bind(HOUSEHOLD)
                .fetch_one(&pool)
                .await
                .unwrap();
        // Committing the same (household, alias, language) twice is a harmless
        // INSERT OR IGNORE no-op — the unique index dedups it.
        commit_via_conn(&pool, MILK_ID, "myuniquemilk", AliasSource::AcceptedMatch).await;
        commit_via_conn(&pool, MILK_ID, "myuniquemilk", AliasSource::Llm).await;
        let after: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases WHERE household_id = ?1")
                .bind(HOUSEHOLD)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after, before + 1, "the second commit is a dedup no-op");
        // The first write wins (source stays accepted_match, not the later llm).
        let (_, source) = alias_lang_source(&pool, MILK_ID, "myuniquemilk").await;
        assert_eq!(source, "accepted_match");
    }

    #[tokio::test]
    async fn commit_alias_skips_empty_normalised_term() {
        let pool = pool().await;
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases")
            .fetch_one(&pool)
            .await
            .unwrap();
        // A term that normalises to empty (only punctuation/emoji) learns nothing.
        commit_via_conn(&pool, MILK_ID, "  !!!  ", AliasSource::AcceptedMatch).await;
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_aliases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, before, "an empty normalised term writes no alias");
    }
}
