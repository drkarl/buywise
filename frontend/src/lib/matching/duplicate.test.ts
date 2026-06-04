import { describe, expect, it } from "vitest";

import { CATEGORY_IDS } from "./dictionary";
import { checkForDuplicate, CONFIDENCE, type ActiveItem } from "./duplicate";

describe("DuplicateDetector", () => {
  describe("CONFIDENCE constants", () => {
    it("has correct confidence values", () => {
      expect(CONFIDENCE.EXACT).toBe(1.0);
      expect(CONFIDENCE.DICTIONARY).toBe(0.95);
      expect(CONFIDENCE.FUZZY).toBe(0.8);
      expect(CONFIDENCE.CONTAINS).toBe(0.7);
    });
  });

  describe("acceptance criteria", () => {
    it('returns confidence 1.0 for exact match: "milk" vs "Milk"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.confidence).toBe(1.0);
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.matchedItem.display_name).toBe("Milk");
      }
    });

    it('returns confidence 0.95 for dictionary match: "leche" vs "Milk"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("leche", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.confidence).toBe(0.95);
        expect(result.duplicate.matchType).toBe("dictionary");
        expect(result.duplicate.matchedItem.display_name).toBe("Milk");
      }
    });

    it('returns confidence 0.8 for fuzzy match: "milks" vs "Milk"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("milks", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.confidence).toBe(0.8);
        expect(result.duplicate.matchType).toBe("fuzzy");
        expect(result.duplicate.matchedItem.display_name).toBe("Milk");
      }
    });

    it('returns confidence 0.7 for contains match: "chocolate chip cookies" vs "chocolate"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "chocolate" }];
      const result = checkForDuplicate("chocolate chip cookies", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.confidence).toBe(0.7);
        expect(result.duplicate.matchType).toBe("contains");
        expect(result.duplicate.matchedItem.display_name).toBe("chocolate");
      }
    });

    it('returns no match with Dairy category suggestion for "eggs" on empty list', () => {
      const result = checkForDuplicate("eggs", []);

      expect(result.duplicate.found).toBe(false);
      expect(result.suggestedCategoryId).toBe(CATEGORY_IDS.DAIRY);
    });
  });

  describe("exact matching", () => {
    it("matches normalized text (case-insensitive)", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Apple" }];

      expect(checkForDuplicate("apple", items).duplicate.found).toBe(true);
      expect(checkForDuplicate("APPLE", items).duplicate.found).toBe(true);
      expect(checkForDuplicate("Apple", items).duplicate.found).toBe(true);
      expect(checkForDuplicate("aPPLe", items).duplicate.found).toBe(true);
    });

    it("matches after removing accents", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Jamón" }];
      const result = checkForDuplicate("jamon", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.confidence).toBe(1.0);
      }
    });

    it("matches after normalizing punctuation to spaces", () => {
      // BuyWise contract: punctuation → space, so "Coca-Cola" normalizes to
      // "coca cola". A spaced or differently-punctuated form of the same text
      // is therefore an exact match (not the old deletion behaviour).
      const items: ActiveItem[] = [{ id: "1", display_name: "Coca-Cola" }];

      const spaced = checkForDuplicate("coca cola", items);
      expect(spaced.duplicate.found).toBe(true);
      if (spaced.duplicate.found) {
        expect(spaced.duplicate.matchType).toBe("exact");
      }

      const hyphenated = checkForDuplicate("Coca-Cola", items);
      expect(hyphenated.duplicate.found).toBe(true);
      if (hyphenated.duplicate.found) {
        expect(hyphenated.duplicate.matchType).toBe("exact");
      }
    });

    it("returns exact match immediately without checking other items", () => {
      const items: ActiveItem[] = [
        { id: "1", display_name: "Water" },
        { id: "2", display_name: "Milk" },
        { id: "3", display_name: "Juice" },
      ];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.matchedItem.id).toBe("2");
      }
    });
  });

  describe("dictionary matching (bilingual)", () => {
    it("matches English to Spanish: leche → milk", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("leche", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
        expect(result.duplicate.confidence).toBe(0.95);
      }
    });

    it("matches Spanish to English: milk → leche", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Leche" }];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
        expect(result.duplicate.confidence).toBe(0.95);
      }
    });

    it("matches variant to English: guineo → banana", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Banana" }];
      const result = checkForDuplicate("guineo", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
      }
    });

    it("matches English to variant: banana → platanos", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Platanos" }];
      const result = checkForDuplicate("banana", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
      }
    });

    it("matches variant to variant: guineo → platanos", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Platanos" }];
      const result = checkForDuplicate("guineo", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
      }
    });

    it("matches Spanish to Spanish for same entry", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Manzana" }];
      const result = checkForDuplicate("manzana", items);

      // This should be exact match since normalized text is identical
      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
      }
    });

    it('matches a hyphenated brand to its spaced variant: "coca-cola" → "Coke"', () => {
      // "coca-cola" normalizes to "coca cola" (punctuation → space), which is the
      // bundled variant of the "cola" entry. "Coke" is also a "cola" variant, so
      // the two resolve to the same canonical → dictionary match. Before the
      // punctuation→space fix, "coca-cola" became "cocacola" and missed entirely.
      const items: ActiveItem[] = [{ id: "1", display_name: "Coke" }];
      const result = checkForDuplicate("coca-cola", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
        expect(result.duplicate.confidence).toBe(0.95);
      }
    });
  });

  describe("quantity-parsed existing items (Finding A)", () => {
    // `display_name` is stored as raw typed text. Existing entries that carry a
    // leading/trailing quantity must be parsed through the same pipeline as the
    // new input, otherwise e.g. "2x milk" normalizes to "2x milk" and never
    // matches a later plain "milk".
    it('warns when active "2x milk" is matched by a later "milk"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "2x milk" }];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.matchedItem.display_name).toBe("2x milk");
      }
    });

    it('warns when active "two eggs" (word number) is matched by a later "eggs"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "two eggs" }];
      const result = checkForDuplicate("eggs", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.matchedItem.display_name).toBe("two eggs");
      }
    });

    it('warns when active "milk 2" (trailing count) is matched by a later "milk"', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "milk 2" }];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
      }
    });
  });

  describe("size constraint (Finding A: size requests don't collapse)", () => {
    // A SIZE request (unit !== null, e.g. "500g") is a constraint dimension and
    // must not collapse into a differently-sized (or unsized) entry. A bare COUNT
    // (unit === null, e.g. "2x") is not a constraint and still collapses.
    it('"500g rice" vs active "1kg rice" → no warning (different size)', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "1kg rice" }];
      const result = checkForDuplicate("500g rice", items);

      expect(result.duplicate.found).toBe(false);
    });

    it('"1kg rice" vs active "rice" (no size) → no warning (size vs no-size)', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "rice" }];
      const result = checkForDuplicate("1kg rice", items);

      expect(result.duplicate.found).toBe(false);
    });

    it('"500g rice" vs active "500g rice" → warning (same size)', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "500g rice" }];
      const result = checkForDuplicate("500g rice", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
      }
    });

    it('regression: counts still collapse — "2x milk" vs active "milk" warns', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "milk" }];
      const result = checkForDuplicate("2x milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
      }
    });

    it('regression: counts still collapse — "3x milk" vs active "2x milk" warns', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "2x milk" }];
      const result = checkForDuplicate("3x milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
      }
    });
  });

  describe("distinct dictionary items are not falsely matched via contains", () => {
    // The dictionary is authoritative: when BOTH terms are dictionary-known but
    // resolve to DIFFERENT canonicals, they are genuinely distinct and must not
    // fall through to the bidirectional contains rule (which would otherwise warn
    // because "bread" is a 5-char substring of "whole wheat bread").
    it('"bread" vs active "whole wheat bread" → no warning', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Whole Wheat Bread" }];
      const result = checkForDuplicate("bread", items);

      expect(result.duplicate.found).toBe(false);
    });

    it('"bread" vs active "white bread" → no warning', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "White Bread" }];
      const result = checkForDuplicate("bread", items);

      expect(result.duplicate.found).toBe(false);
    });

    it('reverse direction: "whole wheat bread" vs active "bread" → no warning', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Bread" }];
      const result = checkForDuplicate("whole wheat bread", items);

      expect(result.duplicate.found).toBe(false);
    });

    // Regression: the fallback must stay active when a dictionary term is only
    // known on ONE side. "oatmeal" is a dictionary entry but "oatmeal cookies"
    // is not, so this still warns via contains (the guard must not over-suppress).
    it('"oatmeal" vs active "oatmeal cookies" still warns via contains', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Oatmeal Cookies" }];
      const result = checkForDuplicate("oatmeal", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("contains");
      }
    });

    it('regression: "leche" vs "Milk" still dictionary-matches', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("leche", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
      }
    });

    it('regression: "milks" vs "Milk" still fuzzy-matches', () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("milks", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("fuzzy");
      }
    });
  });

  describe("fuzzy matching", () => {
    it("matches common typos (distance 1)", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Tomato" }];
      const result = checkForDuplicate("tomatos", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("fuzzy");
        expect(result.duplicate.confidence).toBe(0.8);
      }
    });

    it("matches common typos (distance 2)", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Banana" }];
      const result = checkForDuplicate("bananna", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("fuzzy");
      }
    });

    it("does not match strings that are too different", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("water", items);

      expect(result.duplicate.found).toBe(false);
    });

    it("does not fuzzy match strings shorter than 4 characters", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Ham" }];
      const result = checkForDuplicate("hams", items);

      // "ham" is 3 chars, below minLength for fuzzy matching
      // Should not match via fuzzy
      expect(result.duplicate.found).toBe(false);
    });
  });

  describe("contains matching", () => {
    it("matches when input contains item (substring >= 5 chars)", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Cheese" }];
      const result = checkForDuplicate("cheese pizza", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("contains");
        expect(result.duplicate.confidence).toBe(0.7);
      }
    });

    it("matches when item contains input (substring >= 5 chars)", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Chocolate Milk" }];
      const result = checkForDuplicate("chocolate", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("contains");
      }
    });

    it("does not match substrings shorter than 5 characters", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Cats" }];
      const result = checkForDuplicate("cat food", items);

      // "cats" is 4 chars, below minSubstringLength
      expect(result.duplicate.found).toBe(false);
    });
  });

  describe("priority order", () => {
    it("prefers exact match over dictionary match", () => {
      // "milk" matches exactly with "Milk" and via dictionary with "Leche"
      const items: ActiveItem[] = [
        { id: "1", display_name: "Leche" },
        { id: "2", display_name: "Milk" },
      ];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.matchedItem.id).toBe("2");
      }
    });

    it("prefers dictionary match over fuzzy match", () => {
      const items: ActiveItem[] = [
        { id: "1", display_name: "Milks" }, // fuzzy match for "milk"
        { id: "2", display_name: "Leche" }, // dictionary match for "milk"
      ];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
        expect(result.duplicate.matchedItem.id).toBe("2");
      }
    });

    it("prefers fuzzy match over contains match", () => {
      const items: ActiveItem[] = [
        { id: "1", display_name: "Chocolate Banana" }, // contains "banana"
        { id: "2", display_name: "Bananna" }, // fuzzy match for "banana"
      ];
      const result = checkForDuplicate("banana", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("fuzzy");
        expect(result.duplicate.matchedItem.id).toBe("2");
      }
    });

    it("returns contains match when no better match exists", () => {
      const items: ActiveItem[] = [
        { id: "1", display_name: "Strawberry Ice Cream" },
      ];
      const result = checkForDuplicate("strawberry", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("contains");
      }
    });
  });

  describe("parsed input", () => {
    it("extracts quantity from beginning", () => {
      const result = checkForDuplicate("2 milk", []);
      expect(result.parsed.text).toBe("milk");
      expect(result.parsed.quantity).toBe(2);
      expect(result.parsed.unit).toBeNull();
    });

    it("extracts quantity and unit", () => {
      const result = checkForDuplicate("500g cheese", []);
      expect(result.parsed.text).toBe("cheese");
      expect(result.parsed.quantity).toBe(500);
      expect(result.parsed.unit).toBe("g");
    });

    it("extracts quantity with x notation", () => {
      const result = checkForDuplicate("3x apples", []);
      expect(result.parsed.text).toBe("apples");
      expect(result.parsed.quantity).toBe(3);
    });

    it("preserves original input", () => {
      const result = checkForDuplicate("2x milk", []);
      expect(result.parsed.original).toBe("2x milk");
    });

    it("defaults quantity to 1 when not specified", () => {
      const result = checkForDuplicate("eggs", []);
      expect(result.parsed.quantity).toBe(1);
    });

    it("matches after extracting quantity", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("2x milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
      }
    });
  });

  describe("category suggestion", () => {
    it("suggests Dairy category for milk", () => {
      const result = checkForDuplicate("milk", []);
      expect(result.suggestedCategoryId).toBe(CATEGORY_IDS.DAIRY);
    });

    it("suggests Produce category for apple", () => {
      const result = checkForDuplicate("apple", []);
      expect(result.suggestedCategoryId).toBe(CATEGORY_IDS.PRODUCE);
    });

    it("suggests category for Spanish terms", () => {
      const result = checkForDuplicate("leche", []);
      expect(result.suggestedCategoryId).toBe(CATEGORY_IDS.DAIRY);
    });

    it("suggests category for variants", () => {
      const result = checkForDuplicate("platanos", []);
      expect(result.suggestedCategoryId).toBe(CATEGORY_IDS.PRODUCE);
    });

    it("returns null when term not in dictionary", () => {
      const result = checkForDuplicate("xyzabc123", []);
      expect(result.suggestedCategoryId).toBeNull();
    });

    it("suggests category even when match is found", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      expect(result.suggestedCategoryId).toBe(CATEGORY_IDS.DAIRY);
    });
  });

  describe("edge cases", () => {
    it("handles empty input", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("", items);

      expect(result.duplicate.found).toBe(false);
      expect(result.parsed.text).toBe("");
    });

    it("handles empty active items list", () => {
      const result = checkForDuplicate("milk", []);
      expect(result.duplicate.found).toBe(false);
    });

    it("handles items with empty display_name", () => {
      const items: ActiveItem[] = [
        { id: "1", display_name: "" },
        { id: "2", display_name: "Milk" },
      ];
      const result = checkForDuplicate("milk", items);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchedItem.id).toBe("2");
      }
    });

    it("handles whitespace-only input", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("   ", items);

      expect(result.duplicate.found).toBe(false);
    });

    it("handles input with only quantity", () => {
      const items: ActiveItem[] = [{ id: "1", display_name: "Milk" }];
      const result = checkForDuplicate("2x", items);

      expect(result.duplicate.found).toBe(false);
      expect(result.parsed.text).toBe("");
      expect(result.parsed.quantity).toBe(2);
    });
  });

  describe("integration with mock list data", () => {
    const mockShoppingList: ActiveItem[] = [
      { id: "1", display_name: "Milk" },
      { id: "2", display_name: "Eggs" },
      { id: "3", display_name: "Bread" },
      { id: "4", display_name: "Butter" },
      { id: "5", display_name: "Apples" },
      { id: "6", display_name: "Chicken Breast" },
      { id: "7", display_name: "Brown Rice" },
      { id: "8", display_name: "Olive Oil" },
    ];

    it("finds exact matches in a typical list", () => {
      const result = checkForDuplicate("eggs", mockShoppingList);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("exact");
        expect(result.duplicate.matchedItem.display_name).toBe("Eggs");
      }
    });

    it("finds bilingual matches in a typical list", () => {
      const result = checkForDuplicate("leche", mockShoppingList);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("dictionary");
        expect(result.duplicate.matchedItem.display_name).toBe("Milk");
      }
    });

    it("detects no duplicate for new items", () => {
      const result = checkForDuplicate("tomatoes", mockShoppingList);
      expect(result.duplicate.found).toBe(false);
    });

    it("treats two different known dictionary canonicals as distinct", () => {
      // "chicken" (canonical "chicken") and "Chicken Breast" (canonical
      // "chicken breast") are both dictionary-known but resolve to DIFFERENT
      // canonicals, so they are genuinely distinct — no false contains warning.
      const result = checkForDuplicate("chicken", mockShoppingList);
      expect(result.duplicate.found).toBe(false);
    });

    it("still warns via contains for a multi-word, one-side-unknown item", () => {
      // "Olive Oil" is dictionary-known but "olive oil spray" is not, so the
      // contains fallback stays active and warns.
      const result = checkForDuplicate("olive oil spray", mockShoppingList);

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchType).toBe("contains");
        expect(result.duplicate.matchedItem.display_name).toBe("Olive Oil");
      }
    });
  });

  describe("performance", () => {
    it("handles 100+ items without error", () => {
      // Create a large list
      const items: ActiveItem[] = Array.from({ length: 150 }, (_, i) => ({
        id: String(i),
        display_name: `Item ${i}`,
      }));
      // Add a known match at the end
      items.push({ id: "999", display_name: "Milk" });

      // Smoke test only: run many checks against a large list to catch a
      // pathological blow-up. No wall-clock assertion (flaky on shared CI
      // runners). Assert the final check still finds the planted match, so the
      // loop remains meaningful.
      let result = checkForDuplicate("milk", items);
      for (let i = 0; i < 10; i++) {
        checkForDuplicate("leche", items);
        checkForDuplicate("milks", items);
        checkForDuplicate("chocolate chip cookies", items);
        result = checkForDuplicate("milk", items);
      }

      expect(result.duplicate.found).toBe(true);
      if (result.duplicate.found) {
        expect(result.duplicate.matchedItem.display_name).toBe("Milk");
      }
    });
  });
});
