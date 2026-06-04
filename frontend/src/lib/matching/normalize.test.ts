import { describe, expect, it } from "vitest";

import {
  normalize,
  parseQuantityAndUnit,
  toDisplayName,
  type RequestedSize,
} from "./normalize";

describe("parseQuantityAndUnit", () => {
  // Mirrors the contract of `parse_count_and_size` in crates/core/src/normalise.rs.
  const expectCount = (input: string, text: string, count: number) => {
    expect(parseQuantityAndUnit(input)).toEqual({
      text,
      count,
      requestedSize: null,
      original: input,
    });
  };
  const expectSize = (
    input: string,
    text: string,
    value: number,
    basis: RequestedSize["basis"]
  ) => {
    expect(parseQuantityAndUnit(input)).toEqual({
      text,
      count: 1,
      requestedSize: { value, basis },
      original: input,
    });
  };
  const expectText = (input: string, text: string, original = input) => {
    expect(parseQuantityAndUnit(input)).toEqual({
      text,
      count: 1,
      requestedSize: null,
      original,
    });
  };

  describe("counts (no size constraint — these collapse when matching)", () => {
    it('parses "2x milk"', () => expectCount("2x milk", "milk", 2));
    it('parses "milk x2"', () => expectCount("milk x2", "milk", 2));
    it('parses "3x apples"', () => expectCount("3x apples", "apples", 3));
    it('parses "2 milk"', () => expectCount("2 milk", "milk", 2));
    it('parses "milk 2"', () => expectCount("milk 2", "milk", 2));
    it('parses "5 bananas"', () => expectCount("5 bananas", "bananas", 5));
    it('parses "2 chocolate chip cookies"', () =>
      expectCount("2 chocolate chip cookies", "chocolate chip cookies", 2));
    it("handles very large counts", () =>
      expectCount("1000000 items", "items", 1000000));
  });

  describe("word-number counts (English + Spanish)", () => {
    it('parses "two eggs"', () => expectCount("two eggs", "eggs", 2));
    it('parses "three apples"', () => expectCount("three apples", "apples", 3));
    it('parses "twelve donuts"', () => expectCount("twelve donuts", "donuts", 12));
    it("is case-insensitive", () => expectCount("TWO eggs", "eggs", 2));
    it('parses "dos huevos"', () => expectCount("dos huevos", "huevos", 2));
    it('parses "tres manzanas"', () => expectCount("tres manzanas", "manzanas", 3));
    it('parses "una botella"', () => expectCount("una botella", "botella", 1));
    it('parses "diez naranjas"', () => expectCount("diez naranjas", "naranjas", 10));
  });

  describe("SI size constraints — joined form, converted to base units", () => {
    it('"500g cheese" → 500 g', () => expectSize("500g cheese", "cheese", 500, "g"));
    it('"cheese 500g" → 500 g', () => expectSize("cheese 500g", "cheese", 500, "g"));
    it('"1kg rice" → 1000 g', () => expectSize("1kg rice", "rice", 1000, "g"));
    it('"rice 1kg" → 1000 g', () => expectSize("rice 1kg", "rice", 1000, "g"));
    it('"250ml milk" → 250 ml', () => expectSize("250ml milk", "milk", 250, "ml"));
    it('"2l water" → 2000 ml', () => expectSize("2l water", "water", 2000, "ml"));
    it('"1.5kg flour" → 1500 g', () => expectSize("1.5kg flour", "flour", 1500, "g"));
    it("is unit case-insensitive", () =>
      expectSize("500G cheese", "cheese", 500, "g"));
    it("a lone size yields empty text", () => expectSize("500g", "", 500, "g"));
  });

  describe("SI size constraints — spaced form (incl. Spanish long units)", () => {
    it('"500 g rice" → 500 g', () => expectSize("500 g rice", "rice", 500, "g"));
    it('"dos litros leche" → 2000 ml', () =>
      expectSize("dos litros leche", "leche", 2000, "ml"));
    it('"un litro agua" → 1000 ml', () =>
      expectSize("un litro agua", "agua", 1000, "ml"));
    it('"500 gramos jamon" → 500 g', () =>
      expectSize("500 gramos jamon", "jamon", 500, "g"));
    it('"un kilogramo harina" → 1000 g', () =>
      expectSize("un kilogramo harina", "harina", 1000, "g"));
    it('"100 gramo cafe" → 100 g', () =>
      expectSize("100 gramo cafe", "cafe", 100, "g"));
    it('"250 mililitros nata" → 250 ml', () =>
      expectSize("250 mililitros nata", "nata", 250, "ml"));
    it('"50 centilitros zumo" → 500 ml', () =>
      expectSize("50 centilitros zumo", "zumo", 500, "ml"));
    it('trailing spaced pair: "leche 2 litros" → 2000 ml', () =>
      expectSize("leche 2 litros", "leche", 2000, "ml"));
  });

  describe("imperial / free-text units are NOT sizes (left in the text)", () => {
    // unit_to_basis has no SI mapping for these, so the number+unit token stays
    // in the item text and there is no size constraint.
    it('"1lb butter"', () => expectText("1lb butter", "1lb butter"));
    it('"8oz cream"', () => expectText("8oz cream", "8oz cream"));
    it('"1pt cream"', () => expectText("1pt cream", "1pt cream"));
    it('"2qt broth"', () => expectText("2qt broth", "2qt broth"));
    it('"1gal milk"', () => expectText("1gal milk", "1gal milk"));
    it('"2cups flour"', () => expectText("2cups flour", "2cups flour"));
    it('"1cup sugar"', () => expectText("1cup sugar", "1cup sugar"));
    it('"2tbsp olive oil"', () => expectText("2tbsp olive oil", "2tbsp olive oil"));
    it('"1tsp vanilla"', () => expectText("1tsp vanilla", "1tsp vanilla"));
    it('"2lbs ground beef"', () =>
      expectText("2lbs ground beef", "2lbs ground beef"));
  });

  describe("no count/size (whole line is text)", () => {
    it('parses "milk"', () => expectText("milk", "milk"));
    it('parses "bread and butter"', () =>
      expectText("bread and butter", "bread and butter"));
    it("does not parse numbers embedded in brand names", () =>
      expectText("7up soda", "7up soda"));
    it("a lone count token stays in the text (digit)", () =>
      expectText("2", "2"));
    it("a lone count token stays in the text (word)", () =>
      expectText("two", "two"));
  });

  describe("edge cases", () => {
    it("handles empty string", () => expectText("", ""));
    it("handles whitespace only", () => expectText("   ", "", "   "));
    it("preserves original input with leading/trailing spaces", () =>
      expect(parseQuantityAndUnit("  2 milk  ")).toEqual({
        text: "milk",
        count: 2,
        requestedSize: null,
        original: "  2 milk  ",
      }));
  });
});

describe("normalize", () => {
  describe("lowercase conversion", () => {
    it('converts "LECHE" to "leche"', () => {
      expect(normalize("LECHE")).toBe("leche");
    });

    it('converts "Milk" to "milk"', () => {
      expect(normalize("Milk")).toBe("milk");
    });

    it('converts "MIXED Case TEXT" to "mixed case text"', () => {
      expect(normalize("MIXED Case TEXT")).toBe("mixed case text");
    });
  });

  describe("accent removal", () => {
    it('converts "Jamón" to "jamon"', () => {
      expect(normalize("Jamón")).toBe("jamon");
    });

    it('converts "café" to "cafe"', () => {
      expect(normalize("café")).toBe("cafe");
    });

    it('converts "crème brûlée" to "creme brulee"', () => {
      expect(normalize("crème brûlée")).toBe("creme brulee");
    });

    it('converts "niño" to "nino"', () => {
      expect(normalize("niño")).toBe("nino");
    });

    it('converts "naïve" to "naive"', () => {
      expect(normalize("naïve")).toBe("naive");
    });

    it('converts "Ägypten" to "agypten"', () => {
      expect(normalize("Ägypten")).toBe("agypten");
    });

    it('converts "Müsli" to "musli"', () => {
      expect(normalize("Müsli")).toBe("musli");
    });

    it('converts "açaí" to "acai"', () => {
      expect(normalize("açaí")).toBe("acai");
    });
  });

  describe("punctuation → space (BuyWise core contract)", () => {
    // Mirrors crates/core/src/normalise.rs: punctuation becomes a single SPACE
    // (not deletion), so hyphenated/apostrophised words split into tokens.
    it('converts "coca-cola" to "coca cola"', () => {
      expect(normalize("coca-cola")).toBe("coca cola");
    });

    it("replaces apostrophes with a space", () => {
      expect(normalize("mother's milk")).toBe("mother s milk");
    });

    it("replaces periods with a space (collapsing)", () => {
      expect(normalize("Dr. Pepper")).toBe("dr pepper");
    });

    it("replaces commas with a space", () => {
      expect(normalize("salt, pepper")).toBe("salt pepper");
    });

    it("replaces exclamation marks with a space", () => {
      expect(normalize("Wow!")).toBe("wow");
    });

    it("replaces question marks with a space", () => {
      expect(normalize("milk?")).toBe("milk");
    });

    it("replaces multiple punctuation types with spaces", () => {
      expect(normalize("Hello, World! How's it going?")).toBe(
        "hello world how s it going"
      );
    });
  });

  describe("brand compaction", () => {
    // Brands in BRAND_COMPACTIONS compact instead of splitting on punctuation.
    it('compacts "7-up" to "7up" instead of splitting', () => {
      expect(normalize("7-up")).toBe("7up");
    });

    it("compacts case-insensitively", () => {
      expect(normalize("7-Up")).toBe("7up");
      expect(normalize("7-UP")).toBe("7up");
    });

    it("leaves an already-compacted brand untouched", () => {
      expect(normalize("7up")).toBe("7up");
    });
  });

  describe("whitespace handling", () => {
    it('collapses "  multiple   spaces  " to "multiple spaces"', () => {
      expect(normalize("  multiple   spaces  ")).toBe("multiple spaces");
    });

    it("trims leading spaces", () => {
      expect(normalize("   milk")).toBe("milk");
    });

    it("trims trailing spaces", () => {
      expect(normalize("milk   ")).toBe("milk");
    });

    it("handles tabs", () => {
      expect(normalize("milk\t\tbread")).toBe("milk bread");
    });

    it("handles newlines", () => {
      expect(normalize("milk\n\nbread")).toBe("milk bread");
    });

    it("handles mixed whitespace", () => {
      expect(normalize("  milk \t bread \n eggs  ")).toBe("milk bread eggs");
    });
  });

  describe("edge cases", () => {
    it("handles empty string", () => {
      expect(normalize("")).toBe("");
    });

    it("handles whitespace only", () => {
      expect(normalize("   ")).toBe("");
    });

    it("handles single character", () => {
      expect(normalize("A")).toBe("a");
    });

    it("preserves numbers", () => {
      expect(normalize("7Up")).toBe("7up");
    });

    it("handles numbers with text", () => {
      expect(normalize("Vitamin B12")).toBe("vitamin b12");
    });

    it("handles emoji by removing them", () => {
      expect(normalize("milk 🥛")).toBe("milk");
    });

    it("handles multiple emoji", () => {
      expect(normalize("🍎 apple 🍏")).toBe("apple");
    });

    it("handles emoji-only input", () => {
      expect(normalize("🥛🍞🥚")).toBe("");
    });

    it("handles mixed scripts (Cyrillic)", () => {
      expect(normalize("Молоко")).toBe("молоко");
    });

    it("handles Chinese characters", () => {
      expect(normalize("牛奶")).toBe("牛奶");
    });

    it("handles Japanese characters", () => {
      expect(normalize("ミルク")).toBe("ミルク");
    });

    it("handles combining characters already decomposed", () => {
      // é written as e + combining acute accent
      expect(normalize("café")).toBe("cafe");
    });
  });

  describe("combined transformations", () => {
    it("handles all transformations together", () => {
      expect(normalize("  JAMÓN  con  Café! ")).toBe("jamon con cafe");
    });

    it("normalizes for matching purposes", () => {
      // These should all normalize to the same value
      const variants = ["Jamón", "JAMON", "jamón", "  jamón  ", "JAMÓN!"];
      const normalized = variants.map(normalize);
      expect(new Set(normalized).size).toBe(1);
      expect(normalized[0]).toBe("jamon");
    });
  });
});

describe("toDisplayName", () => {
  it('converts "milk" to "Milk"', () => {
    expect(toDisplayName("milk")).toBe("Milk");
  });

  it("keeps already capitalized first letter", () => {
    expect(toDisplayName("Milk")).toBe("Milk");
  });

  it("does not modify rest of string", () => {
    expect(toDisplayName("mILK")).toBe("MILK");
  });

  it("handles uppercase input", () => {
    expect(toDisplayName("BREAD")).toBe("BREAD");
  });

  it("handles single character", () => {
    expect(toDisplayName("a")).toBe("A");
  });

  it("handles empty string", () => {
    expect(toDisplayName("")).toBe("");
  });

  it("handles multi-word input", () => {
    expect(toDisplayName("whole wheat bread")).toBe("Whole wheat bread");
  });

  it("handles accented first character", () => {
    expect(toDisplayName("éclair")).toBe("Éclair");
  });

  it("handles numbers at start", () => {
    expect(toDisplayName("7up")).toBe("7up");
  });

  it("handles special characters at start", () => {
    expect(toDisplayName("@milk")).toBe("@milk");
  });
});
