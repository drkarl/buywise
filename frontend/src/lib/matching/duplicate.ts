/**
 * DuplicateDetector service for identifying potential duplicate items in a shopping list.
 * Combines normalization, dictionary matching, and fuzzy matching.
 */

import { lookupTerm, type DictionaryEntry, type CategoryId } from "./dictionary";
import { isFuzzySimilar, containsMatchBidirectional } from "./fuzzy";
import { normalize, parseQuantityAndUnit, type ParsedInput } from "./normalize";

/**
 * Confidence scores for different match types.
 * Higher values indicate higher confidence in the match.
 */
export const CONFIDENCE = {
  EXACT: 1.0,
  DICTIONARY: 0.95,
  FUZZY: 0.8,
  CONTAINS: 0.7,
} as const;

/**
 * Match type indicating how a duplicate was detected.
 */
export type MatchType = "exact" | "dictionary" | "fuzzy" | "contains";

/**
 * Information about a detected duplicate match.
 */
export interface DuplicateMatch {
  /** Whether a duplicate was found */
  found: true;
  /** The matching item from the list */
  matchedItem: ActiveItem;
  /** Confidence score (1.0 = exact, 0.95 = dictionary, 0.8 = fuzzy, 0.7 = contains) */
  confidence: number;
  /** Type of match that was detected */
  matchType: MatchType;
}

/**
 * Result when no duplicate is found.
 */
export interface NoDuplicateMatch {
  /** Whether a duplicate was found */
  found: false;
}

/**
 * Union type for duplicate detection result.
 */
export type DuplicateResult = DuplicateMatch | NoDuplicateMatch;

/**
 * Active item in the shopping list for duplicate checking.
 * Only includes fields needed for matching.
 */
export interface ActiveItem {
  id: string;
  display_name: string;
}

/**
 * Complete result from the duplicate detection check.
 */
export interface DuplicateCheckResult {
  /** Parsed input with quantity, unit, and text extracted */
  parsed: ParsedInput;
  /** Duplicate detection result */
  duplicate: DuplicateResult;
  /** Suggested category ID from dictionary lookup (null if not found) */
  suggestedCategoryId: CategoryId | null;
}

/**
 * Gets the dictionary entry for a normalized term.
 *
 * @param normalizedTerm - The normalized term to look up
 * @returns The dictionary entry if found, or null if not in dictionary
 */
function getDictionaryEntry(normalizedTerm: string): DictionaryEntry | null {
  return lookupTerm(normalizedTerm) ?? null;
}

/**
 * Derives a size signature from a parsed input.
 *
 * A bare COUNT (`2x`, `x2`, word-number "two") has `unit === null` and is NOT a
 * constraint — items differing only by count collapse and merge (design.md:209).
 * A SIZE/measure request (`500g`, `1kg`, `1l`) has a non-null `unit` and IS a
 * distinct constraint dimension (design.md:202, 240-246), so it must not collapse
 * into a differently-sized (or unsized) entry.
 *
 * Phase-2 approximation: the signature is the exact `quantity+unit` string, so
 * base-unit conversion (`1kg` == `1000g`) is intentionally NOT applied here — it
 * is deferred to the core normaliser / P3. This only ever SUPPRESSES false
 * positives: a `1kg` vs `1000g` near-miss simply yields two rows that P3
 * reconciles, never a wrong merge.
 *
 * @param p - Parsed input (text, quantity, unit)
 * @returns `"<quantity><unit>"` for a size request, or `null` for a bare count
 */
function sizeKey(p: ParsedInput): string | null {
  return p.unit ? `${p.quantity}${p.unit}` : null;
}

/**
 * Checks for a duplicate item in the active list.
 *
 * Match priority order (highest to lowest confidence):
 * 1. Exact match (normalized) → 1.0
 * 2. Dictionary match (bilingual) → 0.95
 * 3. Fuzzy match (Levenshtein) → 0.8
 * 4. Contains match (substring) → 0.7
 *
 * @param input - The raw text input to check
 * @param activeItems - Array of active items in the current list
 * @returns DuplicateCheckResult with parsed input, duplicate info, and category suggestion
 *
 * @example
 * // Exact match
 * checkForDuplicate("milk", [{ id: "1", display_name: "Milk" }])
 * // → { duplicate: { found: true, confidence: 1.0, matchType: "exact", ... }, ... }
 *
 * @example
 * // Dictionary match (bilingual)
 * checkForDuplicate("leche", [{ id: "1", display_name: "Milk" }])
 * // → { duplicate: { found: true, confidence: 0.95, matchType: "dictionary", ... }, ... }
 *
 * @example
 * // Fuzzy match
 * checkForDuplicate("milks", [{ id: "1", display_name: "Milk" }])
 * // → { duplicate: { found: true, confidence: 0.8, matchType: "fuzzy", ... }, ... }
 *
 * @example
 * // No match with category suggestion
 * checkForDuplicate("eggs", [])
 * // → { duplicate: { found: false }, suggestedCategoryId: "c1000000-...-000000000003", ... }
 */
export function checkForDuplicate(
  input: string,
  activeItems: ActiveItem[]
): DuplicateCheckResult {
  // Parse the input to extract quantity, unit, and text
  const parsed = parseQuantityAndUnit(input);
  const normalizedInput = normalize(parsed.text);

  // Get dictionary entry for category suggestion
  const dictEntry = getDictionaryEntry(normalizedInput);
  const suggestedCategoryId = dictEntry?.categoryId ?? null;

  // Early return if no items to check against
  if (activeItems.length === 0 || !normalizedInput) {
    return {
      parsed,
      duplicate: { found: false },
      suggestedCategoryId,
    };
  }

  // Track best match across all items
  let bestMatch: DuplicateMatch | null = null;

  for (const item of activeItems) {
    // Parse the existing item through the SAME pipeline as the new input.
    // `display_name` is stored as raw typed text, so an entry like "2x milk" or
    // "two eggs" must have its quantity/unit stripped before matching — otherwise
    // it normalizes to "2x milk" and never matches a later plain "milk".
    const parsedItem = parseQuantityAndUnit(item.display_name);
    const normalizedItem = normalize(parsedItem.text);

    if (!normalizedItem) {
      continue;
    }

    // Size is a constraint dimension; a bare count is not. Two items that carry
    // a DIFFERENT size request (or one with a size and one without) are distinct
    // constraints and must never collapse, even when their base text matches.
    // Counts (unit === null on both sides) compare equal here and fall through
    // to the normal match cascade so e.g. "2x milk" still merges with "milk".
    if (sizeKey(parsed) !== sizeKey(parsedItem)) {
      continue;
    }

    // Check match types in priority order
    // 1. Exact match (highest priority)
    if (normalizedInput === normalizedItem) {
      // Exact match is the best possible, return immediately
      return {
        parsed,
        duplicate: {
          found: true,
          matchedItem: item,
          confidence: CONFIDENCE.EXACT,
          matchType: "exact",
        },
        suggestedCategoryId,
      };
    }

    // Resolve both terms in the dictionary once (O(1) Map gets) and reuse the
    // result for the same-canonical match below and the different-canonical
    // negative guard.
    const inputEntry = lookupTerm(normalizedInput);
    const itemEntry = lookupTerm(normalizedItem);

    // 2. Dictionary match (bilingual): both resolve to the SAME canonical.
    if (inputEntry && itemEntry && inputEntry.canonical === itemEntry.canonical) {
      const match: DuplicateMatch = {
        found: true,
        matchedItem: item,
        confidence: CONFIDENCE.DICTIONARY,
        matchType: "dictionary",
      };
      // Keep if it's better than current best
      if (!bestMatch || match.confidence > bestMatch.confidence) {
        bestMatch = match;
      }
      continue; // Don't check lower priority matches for this item
    }

    // Definitive negative: when BOTH terms are dictionary-known but resolve to
    // DIFFERENT canonicals, they are genuinely distinct items (the dictionary is
    // authoritative). Skip the fuzzy/contains fallback for this item so e.g.
    // "bread" doesn't falsely warn against "whole wheat bread" via the
    // bidirectional 5-char contains rule. The fallback is only for unknown/typo'd
    // terms, so it stays active when only one side (or neither) is known.
    if (inputEntry && itemEntry) {
      continue;
    }

    // 3. Fuzzy match (skip if we already have a dictionary match)
    if (!bestMatch || bestMatch.confidence < CONFIDENCE.FUZZY) {
      const fuzzyResult = isFuzzySimilar(normalizedInput, normalizedItem);
      if (fuzzyResult.isSimilar) {
        const match: DuplicateMatch = {
          found: true,
          matchedItem: item,
          confidence: CONFIDENCE.FUZZY,
          matchType: "fuzzy",
        };
        if (!bestMatch || match.confidence > bestMatch.confidence) {
          bestMatch = match;
        }
        continue; // Don't check lower priority matches for this item
      }
    }

    // 4. Contains match (lowest priority)
    if (!bestMatch || bestMatch.confidence < CONFIDENCE.CONTAINS) {
      if (containsMatchBidirectional(normalizedInput, normalizedItem)) {
        const match: DuplicateMatch = {
          found: true,
          matchedItem: item,
          confidence: CONFIDENCE.CONTAINS,
          matchType: "contains",
        };
        if (!bestMatch || match.confidence > bestMatch.confidence) {
          bestMatch = match;
        }
      }
    }
  }

  return {
    parsed,
    duplicate: bestMatch ?? { found: false },
    suggestedCategoryId,
  };
}
