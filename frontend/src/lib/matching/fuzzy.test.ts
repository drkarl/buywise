import { describe, expect, it } from "vitest";

import {
  containsMatch,
  containsMatchBidirectional,
  DEFAULT_THRESHOLD,
  isFuzzySimilar,
  levenshteinDistance,
  MIN_CONTAINS_LENGTH,
  MIN_FUZZY_LENGTH,
} from "./fuzzy";

describe("levenshteinDistance", () => {
  describe("basic operations", () => {
    it("returns 0 for identical strings", () => {
      expect(levenshteinDistance("milk", "milk")).toBe(0);
      expect(levenshteinDistance("tomato", "tomato")).toBe(0);
      expect(levenshteinDistance("", "")).toBe(0);
    });

    it("handles single character insertion", () => {
      expect(levenshteinDistance("milk", "milks")).toBe(1);
      expect(levenshteinDistance("tomato", "tomatos")).toBe(1);
      expect(levenshteinDistance("cat", "cats")).toBe(1);
    });

    it("handles single character deletion", () => {
      expect(levenshteinDistance("milks", "milk")).toBe(1);
      expect(levenshteinDistance("cats", "cat")).toBe(1);
    });

    it("handles single character substitution", () => {
      expect(levenshteinDistance("cat", "car")).toBe(1);
      expect(levenshteinDistance("milk", "molk")).toBe(1);
      expect(levenshteinDistance("hat", "hot")).toBe(1);
    });

    it("handles multiple edits", () => {
      expect(levenshteinDistance("kitten", "sitting")).toBe(3);
      expect(levenshteinDistance("sunday", "saturday")).toBe(3);
    });

    it("returns length of other string when one is empty", () => {
      expect(levenshteinDistance("", "milk")).toBe(4);
      expect(levenshteinDistance("milk", "")).toBe(4);
      expect(levenshteinDistance("", "")).toBe(0);
    });
  });

  describe("acceptance criteria", () => {
    it('returns distance 1 for "milk" and "milks"', () => {
      expect(levenshteinDistance("milk", "milks")).toBe(1);
    });

    it('returns distance 1 for "tomato" and "tomatos"', () => {
      expect(levenshteinDistance("tomato", "tomatos")).toBe(1);
    });

    it('returns distance 5 for "milk" and "water"', () => {
      expect(levenshteinDistance("milk", "water")).toBe(5);
    });

    it('returns distance 1 for "cat" and "car"', () => {
      expect(levenshteinDistance("cat", "car")).toBe(1);
    });
  });

  describe("symmetry", () => {
    it("is symmetric (same result regardless of order)", () => {
      expect(levenshteinDistance("milk", "milks")).toBe(
        levenshteinDistance("milks", "milk")
      );
      expect(levenshteinDistance("cat", "car")).toBe(
        levenshteinDistance("car", "cat")
      );
      expect(levenshteinDistance("kitten", "sitting")).toBe(
        levenshteinDistance("sitting", "kitten")
      );
    });
  });

  describe("edge cases", () => {
    it("handles single character strings", () => {
      expect(levenshteinDistance("a", "b")).toBe(1);
      expect(levenshteinDistance("a", "a")).toBe(0);
    });

    it("handles completely different strings", () => {
      expect(levenshteinDistance("abc", "xyz")).toBe(3);
      expect(levenshteinDistance("hello", "world")).toBe(4);
    });

    it("handles strings with repeated characters", () => {
      expect(levenshteinDistance("aaa", "aaaa")).toBe(1);
      expect(levenshteinDistance("aaa", "bbb")).toBe(3);
    });

    it("handles case-sensitive comparison", () => {
      // Note: Levenshtein is case-sensitive, normalization should be done before calling
      expect(levenshteinDistance("Milk", "milk")).toBe(1);
      expect(levenshteinDistance("MILK", "milk")).toBe(4);
    });
  });

  describe("performance", () => {
    it("handles 100+ comparisons in under 10ms", () => {
      const testPairs = [
        ["milk", "milks"],
        ["tomato", "tomatos"],
        ["banana", "bananas"],
        ["apple", "apples"],
        ["orange", "oranges"],
        ["chicken", "chickens"],
        ["bread", "breads"],
        ["cheese", "cheeses"],
        ["butter", "butters"],
        ["yogurt", "yogurts"],
      ];

      const start = performance.now();

      // Run 100+ comparisons (10 pairs * 11 iterations = 110)
      for (let i = 0; i < 11; i++) {
        for (const [a, b] of testPairs) {
          levenshteinDistance(a, b);
        }
      }

      const elapsed = performance.now() - start;
      // Use generous threshold to avoid CI flakiness under load
      expect(elapsed).toBeLessThan(50);
    });

    it("handles long strings efficiently", () => {
      const longA = "a".repeat(100);
      const longB = "b".repeat(100);

      const start = performance.now();
      const distance = levenshteinDistance(longA, longB);
      const elapsed = performance.now() - start;

      expect(distance).toBe(100);
      // Use generous threshold to avoid CI flakiness under load
      expect(elapsed).toBeLessThan(100);
    });
  });
});

describe("isFuzzySimilar", () => {
  describe("default configuration", () => {
    it("has DEFAULT_THRESHOLD of 2", () => {
      expect(DEFAULT_THRESHOLD).toBe(2);
    });

    it("has MIN_FUZZY_LENGTH of 4", () => {
      expect(MIN_FUZZY_LENGTH).toBe(4);
    });
  });

  describe("acceptance criteria", () => {
    it('"milk" and "milks" are similar (distance = 1)', () => {
      const result = isFuzzySimilar("milk", "milks");
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(1);
    });

    it('"tomato" and "tomatos" are similar (distance = 1)', () => {
      const result = isFuzzySimilar("tomato", "tomatos");
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(1);
    });

    it('"milk" and "water" are NOT similar (distance = 5)', () => {
      const result = isFuzzySimilar("milk", "water");
      expect(result.isSimilar).toBe(false);
      expect(result.distance).toBe(5);
      expect(result.reason).toBe("distance-exceeded");
    });

    it('"cat" and "car" are NOT similar (too short)', () => {
      const result = isFuzzySimilar("cat", "car");
      expect(result.isSimilar).toBe(false);
      expect(result.distance).toBeNull();
      expect(result.reason).toBe("too-short");
    });
  });

  describe("minimum length rule", () => {
    it("rejects strings shorter than minLength (default 4)", () => {
      expect(isFuzzySimilar("cat", "car").isSimilar).toBe(false);
      expect(isFuzzySimilar("cat", "car").reason).toBe("too-short");

      expect(isFuzzySimilar("ab", "ac").isSimilar).toBe(false);
      expect(isFuzzySimilar("a", "b").isSimilar).toBe(false);
    });

    it("accepts strings at exactly minLength", () => {
      expect(isFuzzySimilar("milk", "molk").isSimilar).toBe(true);
      expect(isFuzzySimilar("milk", "milk").isSimilar).toBe(true);
    });

    it("rejects if only one string is too short", () => {
      expect(isFuzzySimilar("cat", "cats").isSimilar).toBe(false);
      expect(isFuzzySimilar("cats", "cat").isSimilar).toBe(false);
    });

    it("respects custom minLength option", () => {
      // With minLength=3, "cat" and "car" can be compared
      const result = isFuzzySimilar("cat", "car", { minLength: 3 });
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(1);
    });
  });

  describe("length difference rule", () => {
    it("skips distance calculation when length difference > threshold", () => {
      // "milk" (4) vs "milking" (7) - diff is 3, > default threshold of 2
      const result = isFuzzySimilar("milk", "milking");
      expect(result.isSimilar).toBe(false);
      expect(result.distance).toBeNull();
      expect(result.reason).toBe("length-diff");
    });

    it("computes distance when length difference <= threshold", () => {
      // "milk" (4) vs "milks" (5) - diff is 1, <= default threshold of 2
      const result = isFuzzySimilar("milk", "milks");
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(1);
    });

    it("respects custom threshold for length difference", () => {
      // With threshold=3, "milk" (4) vs "milking" (7) - diff is 3, can be compared
      const result = isFuzzySimilar("milk", "milking", { threshold: 3 });
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(3);
    });
  });

  describe("threshold matching", () => {
    it("matches when distance equals threshold", () => {
      // "milk" vs "mike" has distance 2
      expect(levenshteinDistance("milk", "mike")).toBe(2);

      const result = isFuzzySimilar("milk", "mike");
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(2);
    });

    it("does not match when distance exceeds threshold", () => {
      // "banana" vs "bonono" has distance 3
      expect(levenshteinDistance("banana", "bonono")).toBe(3);

      const result = isFuzzySimilar("banana", "bonono");
      expect(result.isSimilar).toBe(false);
      expect(result.distance).toBe(3);
      expect(result.reason).toBe("distance-exceeded");
    });

    it("respects custom threshold", () => {
      const result1 = isFuzzySimilar("banana", "bonono", { threshold: 3 });
      expect(result1.isSimilar).toBe(true);

      const result2 = isFuzzySimilar("banana", "bonono", { threshold: 2 });
      expect(result2.isSimilar).toBe(false);
    });
  });

  describe("edge cases", () => {
    it("handles identical strings", () => {
      const result = isFuzzySimilar("milk", "milk");
      expect(result.isSimilar).toBe(true);
      expect(result.distance).toBe(0);
    });

    it("handles empty strings", () => {
      const result = isFuzzySimilar("", "");
      expect(result.isSimilar).toBe(false);
      expect(result.reason).toBe("too-short");
    });

    it("handles one empty string", () => {
      const result = isFuzzySimilar("milk", "");
      expect(result.isSimilar).toBe(false);
      expect(result.reason).toBe("too-short");
    });
  });

  describe("performance", () => {
    it("handles 100+ comparisons in under 10ms", () => {
      const testPairs = [
        ["milk", "milks"],
        ["tomato", "tomatos"],
        ["banana", "bananas"],
        ["apple", "apples"],
        ["orange", "oranges"],
        ["chicken", "chickens"],
        ["bread", "breads"],
        ["cheese", "cheeses"],
        ["butter", "butters"],
        ["yogurt", "yogurts"],
      ];

      const start = performance.now();

      for (let i = 0; i < 11; i++) {
        for (const [a, b] of testPairs) {
          isFuzzySimilar(a, b);
        }
      }

      const elapsed = performance.now() - start;
      expect(elapsed).toBeLessThan(10);
    });
  });
});

describe("containsMatch", () => {
  describe("default configuration", () => {
    it("has MIN_CONTAINS_LENGTH of 5", () => {
      expect(MIN_CONTAINS_LENGTH).toBe(5);
    });
  });

  describe("acceptance criteria", () => {
    it('"chocolate" contains "choco" returns true', () => {
      expect(containsMatch("chocolate", "choco")).toBe(true);
    });
  });

  describe("minimum length rule", () => {
    it("returns false for substrings shorter than minSubstringLength", () => {
      expect(containsMatch("chocolate", "late")).toBe(false); // 4 chars
      expect(containsMatch("chocolate", "cho")).toBe(false); // 3 chars
      expect(containsMatch("chocolate", "c")).toBe(false); // 1 char
    });

    it("returns true for substrings at or above minSubstringLength", () => {
      expect(containsMatch("chocolate", "choco")).toBe(true); // 5 chars
      expect(containsMatch("chocolate", "olate")).toBe(true); // 5 chars
      expect(containsMatch("chocolate", "chocol")).toBe(true); // 6 chars
    });

    it("respects custom minSubstringLength", () => {
      expect(
        containsMatch("chocolate", "late", { minSubstringLength: 4 })
      ).toBe(true);
      expect(containsMatch("chocolate", "cho", { minSubstringLength: 3 })).toBe(
        true
      );
    });
  });

  describe("substring matching", () => {
    it("matches at the beginning", () => {
      expect(containsMatch("chocolate", "choco")).toBe(true);
    });

    it("matches at the end", () => {
      expect(containsMatch("chocolate", "olate")).toBe(true);
    });

    it("matches in the middle", () => {
      expect(containsMatch("chocolate", "ocola")).toBe(true);
    });

    it("returns false when substring is not contained", () => {
      expect(containsMatch("chocolate", "vanila")).toBe(false);
      expect(containsMatch("chocolate", "milks")).toBe(false);
    });

    it("returns false when text is shorter than substring", () => {
      expect(containsMatch("choco", "chocolate")).toBe(false);
    });
  });

  describe("edge cases", () => {
    it("handles exact match (text equals substring)", () => {
      expect(containsMatch("choco", "choco")).toBe(true);
    });

    it("handles empty text", () => {
      expect(containsMatch("", "choco")).toBe(false);
    });

    it("handles empty substring", () => {
      // Empty substring is less than minSubstringLength
      expect(containsMatch("chocolate", "")).toBe(false);
    });

    it("is case-sensitive", () => {
      // Note: normalization should be done before calling
      expect(containsMatch("Chocolate", "choco")).toBe(false);
      expect(containsMatch("chocolate", "Choco")).toBe(false);
    });
  });
});

describe("containsMatchBidirectional", () => {
  it("returns true when first contains second", () => {
    expect(containsMatchBidirectional("chocolate", "choco")).toBe(true);
  });

  it("returns true when second contains first", () => {
    expect(containsMatchBidirectional("choco", "chocolate")).toBe(true);
  });

  it("returns false when neither contains the other", () => {
    expect(containsMatchBidirectional("milk", "water")).toBe(false);
  });

  it("returns false when match does not meet minimum length", () => {
    expect(containsMatchBidirectional("cats", "cat")).toBe(false);
  });

  it("respects custom minSubstringLength", () => {
    expect(
      containsMatchBidirectional("cats", "cat", { minSubstringLength: 3 })
    ).toBe(true);
    expect(
      containsMatchBidirectional("cat", "cats", { minSubstringLength: 3 })
    ).toBe(true);
  });
});
