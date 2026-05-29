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
//! ## Bilingual / dual-language seed
//!
//! `item_aliases.language` is an input-locale *matching bucket*, not a
//! linguistic assertion: the seed inserts the same variant under BOTH `'en'`
//! and `'es'` for one item. Language is therefore treated as a ranking hint
//! only — never a filter — and every query groups by `canonical_item_id` so the
//! dual-inserted rows of one item count once.
//!
//! ## Thresholds (ported from Shoppy `src/lib/matching/fuzzy.ts`)
//!
//! * [`FUZZY_DISTANCE_THRESHOLD`] = 2 — max Levenshtein distance.
//! * [`MIN_FUZZY_LENGTH`] = 4 — both strings must be at least this long to be
//!   compared by edit distance.
//! * [`MIN_CONTAINS_LENGTH`] = 5 — the shorter string must be at least this long
//!   for a bidirectional substring (`contains`) match.

use std::collections::BTreeSet;

use sqlx::sqlite::SqlitePool;
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
/// The query selects `DISTINCT canonical_item_id`, so the dual-language seed
/// (the same alias stored under both `'en'` and `'es'` for one item) collapses
/// to a single row and is never double-counted. Language is not used as a
/// filter.
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

    let aliases: Vec<(String, String)> =
        sqlx::query_as("SELECT canonical_item_id, alias FROM item_aliases WHERE household_id = ?1")
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

    fn single(outcome: ResolutionOutcome) -> String {
        match outcome {
            ResolutionOutcome::SingleMatch { canonical_item_id } => canonical_item_id,
            other => panic!("expected SingleMatch, got {other:?}"),
        }
    }

    // ---- in-line unit rules (no DB) -----------------------------------------

    #[test]
    fn fuzzy_levenshtein_boundary() {
        // Distance exactly 2 matches; 3 does not (both strings ≥ 4 long).
        assert!(is_fuzzy_similar("melon", "lemon")); // distance 2 (swap m/l)
        assert!(is_fuzzy_similar("milks", "milk")); // distance 1
        assert!(is_fuzzy_similar("tomato", "tomatos")); // distance 1
        assert!(!is_fuzzy_similar("milk", "water")); // distance 4
                                                     // Construct an exact-distance-3 pair: "abcd" vs "abxyz" — but keep ≥4.
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
}
