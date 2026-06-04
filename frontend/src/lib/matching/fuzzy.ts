/**
 * Fuzzy matching utilities using Levenshtein distance.
 * Used for approximate string matching in grocery item comparison.
 */

/**
 * Default threshold for fuzzy matching (maximum Levenshtein distance).
 */
export const DEFAULT_THRESHOLD = 2;

/**
 * Minimum string length for fuzzy matching.
 * Shorter strings are too prone to false positives.
 */
export const MIN_FUZZY_LENGTH = 4;

/**
 * Minimum substring length for contains matching.
 */
export const MIN_CONTAINS_LENGTH = 5;

/**
 * Computes the Levenshtein distance between two strings.
 * Uses Wagner-Fischer algorithm with O(min(m,n)) space optimization.
 *
 * @param a - First string
 * @param b - Second string
 * @returns The minimum number of single-character edits (insertions, deletions, substitutions)
 *
 * @example
 * levenshteinDistance("milk", "milks") // 1
 * levenshteinDistance("tomato", "tomatos") // 1
 * levenshteinDistance("milk", "water") // 5
 */
export function levenshteinDistance(a: string, b: string): number {
  // Handle empty string cases
  if (a.length === 0) return b.length;
  if (b.length === 0) return a.length;

  // Ensure a is the shorter string for space optimization
  if (a.length > b.length) {
    [a, b] = [b, a];
  }

  const m = a.length;
  const n = b.length;

  // Use single array with O(min(m,n)) space
  let previousRow = new Array<number>(m + 1).fill(0);
  let currentRow = new Array<number>(m + 1).fill(0);

  // Initialize first row
  for (let i = 0; i <= m; i++) {
    previousRow[i] = i;
  }

  // Fill in the rest of the matrix
  for (let j = 1; j <= n; j++) {
    currentRow[0] = j;

    for (let i = 1; i <= m; i++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1;

      currentRow[i] = Math.min(
        previousRow[i] + 1, // deletion
        currentRow[i - 1] + 1, // insertion
        previousRow[i - 1] + cost // substitution
      );
    }

    // Swap rows
    [previousRow, currentRow] = [currentRow, previousRow];
  }

  return previousRow[m];
}

/**
 * Options for fuzzy similarity checking.
 */
export interface FuzzySimilarityOptions {
  /**
   * Maximum Levenshtein distance to consider strings similar.
   * @default 2
   */
  threshold?: number;

  /**
   * Minimum string length for fuzzy matching.
   * Strings shorter than this are never considered similar via fuzzy matching.
   * @default 4
   */
  minLength?: number;
}

/**
 * Result of a similarity check.
 */
export interface SimilarityResult {
  /**
   * Whether the strings are considered similar.
   */
  isSimilar: boolean;

  /**
   * The computed Levenshtein distance (if fuzzy matching was performed).
   */
  distance: number | null;

  /**
   * Reason for the result when not similar.
   */
  reason?: "too-short" | "length-diff" | "distance-exceeded";
}

/**
 * Checks if two strings are similar using fuzzy matching.
 *
 * Rules:
 * 1. Strings shorter than minLength are never matched (too short)
 * 2. If length difference exceeds threshold, skip distance calculation
 * 3. Otherwise, compute Levenshtein distance and compare to threshold
 *
 * @param a - First string (should be normalized)
 * @param b - Second string (should be normalized)
 * @param options - Configuration options
 * @returns Similarity result with distance information
 *
 * @example
 * isFuzzySimilar("milk", "milks") // { isSimilar: true, distance: 1 }
 * isFuzzySimilar("cat", "car") // { isSimilar: false, distance: null, reason: "too-short" }
 * isFuzzySimilar("milk", "chocolate") // { isSimilar: false, distance: null, reason: "length-diff" }
 */
export function isFuzzySimilar(
  a: string,
  b: string,
  options: FuzzySimilarityOptions = {}
): SimilarityResult {
  const { threshold = DEFAULT_THRESHOLD, minLength = MIN_FUZZY_LENGTH } =
    options;

  // Rule 1: Skip if either string is too short
  if (a.length < minLength || b.length < minLength) {
    return {
      isSimilar: false,
      distance: null,
      reason: "too-short",
    };
  }

  // Rule 2: Skip if length difference exceeds threshold
  const lengthDiff = Math.abs(a.length - b.length);
  if (lengthDiff > threshold) {
    return {
      isSimilar: false,
      distance: null,
      reason: "length-diff",
    };
  }

  // Rule 3: Compute distance and compare
  const distance = levenshteinDistance(a, b);

  if (distance <= threshold) {
    return {
      isSimilar: true,
      distance,
    };
  }

  return {
    isSimilar: false,
    distance,
    reason: "distance-exceeded",
  };
}

/**
 * Options for contains matching.
 */
export interface ContainsMatchOptions {
  /**
   * Minimum length for the substring to be considered a valid match.
   * @default 5
   */
  minSubstringLength?: number;
}

/**
 * Checks if one string contains another as a substring match.
 * Used for partial matching (e.g., "chocolate" contains "choco").
 *
 * @param text - The text to search in
 * @param substring - The substring to search for
 * @param options - Configuration options
 * @returns True if text contains substring and substring meets minimum length
 *
 * @example
 * containsMatch("chocolate", "choco") // true
 * containsMatch("chocolate", "late") // false (only 4 chars)
 * containsMatch("chocolate", "olate") // true (5 chars)
 */
export function containsMatch(
  text: string,
  substring: string,
  options: ContainsMatchOptions = {}
): boolean {
  const { minSubstringLength = MIN_CONTAINS_LENGTH } = options;

  // Substring must meet minimum length requirement
  if (substring.length < minSubstringLength) {
    return false;
  }

  return text.includes(substring);
}

/**
 * Bidirectional contains check - returns true if either string contains the other.
 *
 * @param a - First string
 * @param b - Second string
 * @param options - Configuration options
 * @returns True if either string contains the other as a valid substring
 *
 * @example
 * containsMatchBidirectional("chocolate", "choco") // true
 * containsMatchBidirectional("choco", "chocolate") // true
 */
export function containsMatchBidirectional(
  a: string,
  b: string,
  options: ContainsMatchOptions = {}
): boolean {
  return containsMatch(a, b, options) || containsMatch(b, a, options);
}
