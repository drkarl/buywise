import { describe, expect, it } from "vitest";

import {
  CATEGORY_IDS,
  DICTIONARY,
  getEntriesByCategory,
  getDictionaryStats,
  lookupByEnglish,
  lookupBySpanish,
  lookupByVariant,
  lookupTerm,
} from "./dictionary";

describe("dictionary", () => {
  describe("DICTIONARY size requirements", () => {
    it("contains at least 200 entries", () => {
      expect(DICTIONARY.length).toBeGreaterThanOrEqual(200);
    });

    it("all entries have required fields", () => {
      for (const entry of DICTIONARY) {
        expect(entry.termEn).toBeDefined();
        expect(entry.termEn.length).toBeGreaterThan(0);
        expect(entry.termEs).toBeDefined();
        expect(entry.termEs.length).toBeGreaterThan(0);
        expect(entry.canonical).toBeDefined();
        expect(entry.canonical.length).toBeGreaterThan(0);
        expect(entry.categoryId).toBeDefined();
      }
    });
  });

  describe("CATEGORY_IDS", () => {
    it("contains all 11 categories", () => {
      const categoryCount = Object.keys(CATEGORY_IDS).length;
      expect(categoryCount).toBe(11);
    });

    it("has correct category IDs matching database seed", () => {
      expect(CATEGORY_IDS.PRODUCE).toBe("c1000000-0000-0000-0000-000000000001");
      expect(CATEGORY_IDS.MEAT_FISH).toBe(
        "c1000000-0000-0000-0000-000000000002"
      );
      expect(CATEGORY_IDS.DAIRY).toBe("c1000000-0000-0000-0000-000000000003");
      expect(CATEGORY_IDS.BAKERY).toBe("c1000000-0000-0000-0000-000000000004");
      expect(CATEGORY_IDS.PANTRY).toBe("c1000000-0000-0000-0000-000000000005");
      expect(CATEGORY_IDS.FROZEN).toBe("c1000000-0000-0000-0000-000000000006");
      expect(CATEGORY_IDS.SNACKS).toBe("c1000000-0000-0000-0000-000000000007");
      expect(CATEGORY_IDS.DRINKS).toBe("c1000000-0000-0000-0000-000000000008");
      expect(CATEGORY_IDS.BABY).toBe("c1000000-0000-0000-0000-000000000009");
      expect(CATEGORY_IDS.HOUSEHOLD).toBe(
        "c1000000-0000-0000-0000-000000000010"
      );
      expect(CATEGORY_IDS.TOILETRIES).toBe(
        "c1000000-0000-0000-0000-000000000011"
      );
    });

    it("all dictionary entries reference valid category IDs", () => {
      const validCategoryIds = new Set(Object.values(CATEGORY_IDS));

      for (const entry of DICTIONARY) {
        expect(validCategoryIds.has(entry.categoryId)).toBe(true);
      }
    });
  });

  describe("dictionary balance", () => {
    it("has entries in all 11 categories", () => {
      const stats = getDictionaryStats();
      const categoryCount = Object.keys(stats.byCategory).length;
      expect(categoryCount).toBe(11);
    });

    it("each category has at least 10 entries", () => {
      const stats = getDictionaryStats();

      for (const count of Object.values(stats.byCategory)) {
        expect(count).toBeGreaterThanOrEqual(10);
      }
    });

    it("no single category dominates (max 20% of total)", () => {
      const stats = getDictionaryStats();
      const maxAllowed = stats.total * 0.2;

      for (const count of Object.values(stats.byCategory)) {
        expect(count).toBeLessThanOrEqual(maxAllowed);
      }
    });
  });

  describe("lookupByEnglish", () => {
    it('returns correct entry for "milk"', () => {
      const entry = lookupByEnglish("milk");

      expect(entry).toBeDefined();
      expect(entry?.termEn).toBe("milk");
      expect(entry?.termEs).toBe("leche");
      expect(entry?.canonical).toBe("milk");
      expect(entry?.categoryId).toBe(CATEGORY_IDS.DAIRY);
    });

    it("is case insensitive", () => {
      const entry1 = lookupByEnglish("Milk");
      const entry2 = lookupByEnglish("MILK");
      const entry3 = lookupByEnglish("milk");

      expect(entry1).toEqual(entry2);
      expect(entry2).toEqual(entry3);
    });

    it("returns undefined for non-existent term", () => {
      const entry = lookupByEnglish("nonexistent");
      expect(entry).toBeUndefined();
    });

    it("provides O(1) lookup time", () => {
      // Run multiple lookups to verify consistent performance
      const terms = ["milk", "bread", "chicken", "apple", "water"];
      const start = performance.now();

      for (let i = 0; i < 10000; i++) {
        for (const term of terms) {
          lookupByEnglish(term);
        }
      }

      const elapsed = performance.now() - start;
      // Sanity check: 50000 lookups should complete in under 100ms for O(1) operations
      // Note: This is a sanity check, not a strict benchmark - may vary in CI environments
      expect(elapsed).toBeLessThan(100);
    });
  });

  describe("lookupBySpanish", () => {
    it('returns correct entry for "leche"', () => {
      const entry = lookupBySpanish("leche");

      expect(entry).toBeDefined();
      expect(entry?.termEn).toBe("milk");
      expect(entry?.termEs).toBe("leche");
      expect(entry?.canonical).toBe("milk");
    });

    it("is case insensitive", () => {
      const entry1 = lookupBySpanish("Leche");
      const entry2 = lookupBySpanish("LECHE");
      const entry3 = lookupBySpanish("leche");

      expect(entry1).toEqual(entry2);
      expect(entry2).toEqual(entry3);
    });

    it("returns undefined for non-existent term", () => {
      const entry = lookupBySpanish("inexistente");
      expect(entry).toBeUndefined();
    });

    it("provides O(1) lookup time", () => {
      const terms = ["leche", "pan", "pollo", "manzana", "agua"];
      const start = performance.now();

      for (let i = 0; i < 10000; i++) {
        for (const term of terms) {
          lookupBySpanish(term);
        }
      }

      const elapsed = performance.now() - start;
      // Sanity check for O(1) performance - may vary in CI environments
      expect(elapsed).toBeLessThan(100);
    });
  });

  describe("lookupByVariant", () => {
    it("returns entry for plural variants", () => {
      const entry = lookupByVariant("apples");

      expect(entry).toBeDefined();
      expect(entry?.termEn).toBe("apple");
      expect(entry?.canonical).toBe("apple");
    });

    it("returns entry for Spanish plural variants", () => {
      const entry = lookupByVariant("manzanas");

      expect(entry).toBeDefined();
      expect(entry?.termEn).toBe("apple");
    });

    it("returns entry for regional variants (potato)", () => {
      const entry1 = lookupByVariant("potatoes");
      const entry2 = lookupByVariant("papas");
      const entry3 = lookupByVariant("patata");
      const entry4 = lookupByVariant("patatas");

      expect(entry1?.canonical).toBe("potato");
      expect(entry2?.canonical).toBe("potato");
      expect(entry3?.canonical).toBe("potato");
      expect(entry4?.canonical).toBe("potato");

      // All variants should map to the same entry
      expect(entry1).toEqual(entry2);
      expect(entry2).toEqual(entry3);
      expect(entry3).toEqual(entry4);
    });

    it("is case insensitive", () => {
      const entry1 = lookupByVariant("Potatoes");
      const entry2 = lookupByVariant("POTATOES");
      const entry3 = lookupByVariant("potatoes");

      expect(entry1).toEqual(entry2);
      expect(entry2).toEqual(entry3);
    });

    it("returns undefined for non-existent variant", () => {
      const entry = lookupByVariant("nonexistent");
      expect(entry).toBeUndefined();
    });
  });

  describe("lookupTerm", () => {
    it("finds entry by English term", () => {
      const entry = lookupTerm("milk");
      expect(entry?.canonical).toBe("milk");
    });

    it("finds entry by Spanish term", () => {
      const entry = lookupTerm("leche");
      expect(entry?.canonical).toBe("milk");
    });

    it("finds entry by variant", () => {
      const entry = lookupTerm("potatoes");
      expect(entry?.canonical).toBe("potato");
    });

    it("is case insensitive", () => {
      expect(lookupTerm("MILK")?.canonical).toBe("milk");
      expect(lookupTerm("Leche")?.canonical).toBe("milk");
      expect(lookupTerm("POTATOES")?.canonical).toBe("potato");
    });

    it("returns undefined for non-existent term", () => {
      const entry = lookupTerm("xyz123");
      expect(entry).toBeUndefined();
    });
  });

  describe("getEntriesByCategory", () => {
    it("returns all produce entries", () => {
      const entries = getEntriesByCategory(CATEGORY_IDS.PRODUCE);

      expect(entries.length).toBeGreaterThan(0);
      for (const entry of entries) {
        expect(entry.categoryId).toBe(CATEGORY_IDS.PRODUCE);
      }
    });

    it("returns different entries for different categories", () => {
      const produce = getEntriesByCategory(CATEGORY_IDS.PRODUCE);
      const dairy = getEntriesByCategory(CATEGORY_IDS.DAIRY);

      expect(produce.length).toBeGreaterThan(0);
      expect(dairy.length).toBeGreaterThan(0);

      // No overlap
      const produceTerms = new Set(produce.map((e) => e.termEn));
      for (const entry of dairy) {
        expect(produceTerms.has(entry.termEn)).toBe(false);
      }
    });
  });

  describe("getDictionaryStats", () => {
    it("returns total count matching DICTIONARY length", () => {
      const stats = getDictionaryStats();
      expect(stats.total).toBe(DICTIONARY.length);
    });

    it("returns counts for all categories", () => {
      const stats = getDictionaryStats();
      const categoryCount = Object.keys(stats.byCategory).length;
      expect(categoryCount).toBe(11);
    });

    it("category counts sum to total", () => {
      const stats = getDictionaryStats();
      const sum = Object.values(stats.byCategory).reduce((a, b) => a + b, 0);
      expect(sum).toBe(stats.total);
    });
  });

  describe("specific entries (acceptance criteria)", () => {
    it("milk entry has correct structure", () => {
      const entry = lookupByEnglish("milk");

      expect(entry).toMatchObject({
        termEn: "milk",
        termEs: "leche",
        canonical: "milk",
        categoryId: CATEGORY_IDS.DAIRY,
      });
    });

    it("potato variants all map to same canonical term", () => {
      // These should all resolve to the same entry
      const variants = [
        "potato",
        "potatoes",
        "papa",
        "papas",
        "patata",
        "patatas",
      ];
      const entries = variants.map((v) => lookupTerm(v));

      // All should be defined
      for (const entry of entries) {
        expect(entry).toBeDefined();
      }

      // All should have the same canonical
      const canonicals = new Set(entries.map((e) => e?.canonical));
      expect(canonicals.size).toBe(1);
      expect(canonicals.has("potato")).toBe(true);
    });

    it("English lookup returns entry in O(1) time", () => {
      // This is verified by the Map implementation
      // We test that multiple sequential lookups are fast
      const iterations = 100000;
      const start = performance.now();

      for (let i = 0; i < iterations; i++) {
        lookupByEnglish("milk");
      }

      const elapsed = performance.now() - start;
      // 100k lookups should be very fast with O(1) Map
      expect(elapsed).toBeLessThan(50);
    });

    it("Spanish lookup returns same entry as English lookup", () => {
      const byEn = lookupByEnglish("milk");
      const byEs = lookupBySpanish("leche");

      expect(byEn).toEqual(byEs);
    });
  });

  describe("data quality", () => {
    it("no duplicate English terms", () => {
      const englishTerms = DICTIONARY.map((e) => e.termEn.toLowerCase());
      const uniqueTerms = new Set(englishTerms);
      expect(uniqueTerms.size).toBe(englishTerms.length);
    });

    it("no duplicate Spanish terms", () => {
      const spanishTerms = DICTIONARY.map((e) => e.termEs.toLowerCase());
      const uniqueTerms = new Set(spanishTerms);
      expect(uniqueTerms.size).toBe(spanishTerms.length);
    });

    it("canonical matches English term", () => {
      for (const entry of DICTIONARY) {
        expect(entry.canonical).toBe(entry.termEn);
      }
    });

    it("variants do not duplicate primary terms", () => {
      const primaryTerms = new Set<string>();
      for (const entry of DICTIONARY) {
        primaryTerms.add(entry.termEn.toLowerCase());
        primaryTerms.add(entry.termEs.toLowerCase());
      }

      for (const entry of DICTIONARY) {
        if (entry.variants) {
          for (const variant of entry.variants) {
            // Variants shouldn't be primary terms of OTHER entries
            // (they can be related to their own entry's primary terms)
            const isOwnTerm =
              variant.toLowerCase() === entry.termEn.toLowerCase() ||
              variant.toLowerCase() === entry.termEs.toLowerCase();

            if (!isOwnTerm) {
              const isPrimaryOfOther = DICTIONARY.some(
                (other) =>
                  other !== entry &&
                  (other.termEn.toLowerCase() === variant.toLowerCase() ||
                    other.termEs.toLowerCase() === variant.toLowerCase())
              );
              expect(isPrimaryOfOther).toBe(false);
            }
          }
        }
      }
    });
  });
});
