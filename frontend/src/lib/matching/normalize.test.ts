import { describe, expect, it } from "vitest";

import { normalize, parseQuantityAndUnit, toDisplayName } from "./normalize";

describe("parseQuantityAndUnit", () => {
  describe("quantity with x pattern", () => {
    it('parses "2x milk" correctly', () => {
      const result = parseQuantityAndUnit("2x milk");
      expect(result).toEqual({
        text: "milk",
        quantity: 2,
        unit: null,
        original: "2x milk",
      });
    });

    it('parses "milk x2" correctly', () => {
      const result = parseQuantityAndUnit("milk x2");
      expect(result).toEqual({
        text: "milk",
        quantity: 2,
        unit: null,
        original: "milk x2",
      });
    });

    it('parses "3x apples" correctly', () => {
      const result = parseQuantityAndUnit("3x apples");
      expect(result).toEqual({
        text: "apples",
        quantity: 3,
        unit: null,
        original: "3x apples",
      });
    });
  });

  describe("quantity with space pattern", () => {
    it('parses "2 milk" correctly', () => {
      const result = parseQuantityAndUnit("2 milk");
      expect(result).toEqual({
        text: "milk",
        quantity: 2,
        unit: null,
        original: "2 milk",
      });
    });

    it('parses "milk 2" correctly', () => {
      const result = parseQuantityAndUnit("milk 2");
      expect(result).toEqual({
        text: "milk",
        quantity: 2,
        unit: null,
        original: "milk 2",
      });
    });

    it('parses "5 bananas" correctly', () => {
      const result = parseQuantityAndUnit("5 bananas");
      expect(result).toEqual({
        text: "bananas",
        quantity: 5,
        unit: null,
        original: "5 bananas",
      });
    });
  });

  describe("quantity with unit", () => {
    it('parses "500g cheese" correctly', () => {
      const result = parseQuantityAndUnit("500g cheese");
      expect(result).toEqual({
        text: "cheese",
        quantity: 500,
        unit: "g",
        original: "500g cheese",
      });
    });

    it('parses "cheese 500g" correctly', () => {
      const result = parseQuantityAndUnit("cheese 500g");
      expect(result).toEqual({
        text: "cheese",
        quantity: 500,
        unit: "g",
        original: "cheese 500g",
      });
    });

    it('parses "1kg rice" correctly', () => {
      const result = parseQuantityAndUnit("1kg rice");
      expect(result).toEqual({
        text: "rice",
        quantity: 1,
        unit: "kg",
        original: "1kg rice",
      });
    });

    it('parses "rice 1kg" correctly', () => {
      const result = parseQuantityAndUnit("rice 1kg");
      expect(result).toEqual({
        text: "rice",
        quantity: 1,
        unit: "kg",
        original: "rice 1kg",
      });
    });

    it('parses "250ml milk" correctly', () => {
      const result = parseQuantityAndUnit("250ml milk");
      expect(result).toEqual({
        text: "milk",
        quantity: 250,
        unit: "ml",
        original: "250ml milk",
      });
    });

    it('parses "2l water" correctly', () => {
      const result = parseQuantityAndUnit("2l water");
      expect(result).toEqual({
        text: "water",
        quantity: 2,
        unit: "l",
        original: "2l water",
      });
    });

    it('parses "1lb butter" correctly', () => {
      const result = parseQuantityAndUnit("1lb butter");
      expect(result).toEqual({
        text: "butter",
        quantity: 1,
        unit: "lb",
        original: "1lb butter",
      });
    });

    it('parses "8oz cream" correctly', () => {
      const result = parseQuantityAndUnit("8oz cream");
      expect(result).toEqual({
        text: "cream",
        quantity: 8,
        unit: "oz",
        original: "8oz cream",
      });
    });

    it('parses "1pt cream" correctly', () => {
      const result = parseQuantityAndUnit("1pt cream");
      expect(result).toEqual({
        text: "cream",
        quantity: 1,
        unit: "pt",
        original: "1pt cream",
      });
    });

    it('parses "2qt broth" correctly', () => {
      const result = parseQuantityAndUnit("2qt broth");
      expect(result).toEqual({
        text: "broth",
        quantity: 2,
        unit: "qt",
        original: "2qt broth",
      });
    });

    it('parses "1gal milk" correctly', () => {
      const result = parseQuantityAndUnit("1gal milk");
      expect(result).toEqual({
        text: "milk",
        quantity: 1,
        unit: "gal",
        original: "1gal milk",
      });
    });

    it('parses "2cups flour" correctly', () => {
      const result = parseQuantityAndUnit("2cups flour");
      expect(result).toEqual({
        text: "flour",
        quantity: 2,
        unit: "cups",
        original: "2cups flour",
      });
    });

    it('parses "1cup sugar" correctly', () => {
      const result = parseQuantityAndUnit("1cup sugar");
      expect(result).toEqual({
        text: "sugar",
        quantity: 1,
        unit: "cup",
        original: "1cup sugar",
      });
    });

    it('parses "2tbsp olive oil" correctly', () => {
      const result = parseQuantityAndUnit("2tbsp olive oil");
      expect(result).toEqual({
        text: "olive oil",
        quantity: 2,
        unit: "tbsp",
        original: "2tbsp olive oil",
      });
    });

    it('parses "1tsp vanilla" correctly', () => {
      const result = parseQuantityAndUnit("1tsp vanilla");
      expect(result).toEqual({
        text: "vanilla",
        quantity: 1,
        unit: "tsp",
        original: "1tsp vanilla",
      });
    });
  });

  describe("English word numbers", () => {
    it('parses "two eggs" correctly', () => {
      const result = parseQuantityAndUnit("two eggs");
      expect(result).toEqual({
        text: "eggs",
        quantity: 2,
        unit: null,
        original: "two eggs",
      });
    });

    it('parses "three apples" correctly', () => {
      const result = parseQuantityAndUnit("three apples");
      expect(result).toEqual({
        text: "apples",
        quantity: 3,
        unit: null,
        original: "three apples",
      });
    });

    it('parses "twelve donuts" correctly', () => {
      const result = parseQuantityAndUnit("twelve donuts");
      expect(result).toEqual({
        text: "donuts",
        quantity: 12,
        unit: null,
        original: "twelve donuts",
      });
    });

    it("handles case insensitivity for word numbers", () => {
      const result = parseQuantityAndUnit("TWO eggs");
      expect(result).toEqual({
        text: "eggs",
        quantity: 2,
        unit: null,
        original: "TWO eggs",
      });
    });
  });

  describe("Spanish word numbers", () => {
    it('parses "dos huevos" correctly', () => {
      const result = parseQuantityAndUnit("dos huevos");
      expect(result).toEqual({
        text: "huevos",
        quantity: 2,
        unit: null,
        original: "dos huevos",
      });
    });

    it('parses "tres manzanas" correctly', () => {
      const result = parseQuantityAndUnit("tres manzanas");
      expect(result).toEqual({
        text: "manzanas",
        quantity: 3,
        unit: null,
        original: "tres manzanas",
      });
    });

    it('parses "una botella" correctly', () => {
      const result = parseQuantityAndUnit("una botella");
      expect(result).toEqual({
        text: "botella",
        quantity: 1,
        unit: null,
        original: "una botella",
      });
    });

    it('parses "diez naranjas" correctly', () => {
      const result = parseQuantityAndUnit("diez naranjas");
      expect(result).toEqual({
        text: "naranjas",
        quantity: 10,
        unit: null,
        original: "diez naranjas",
      });
    });
  });

  describe("no quantity (default to 1)", () => {
    it('parses "milk" with default quantity 1', () => {
      const result = parseQuantityAndUnit("milk");
      expect(result).toEqual({
        text: "milk",
        quantity: 1,
        unit: null,
        original: "milk",
      });
    });

    it('parses "bread and butter" with default quantity 1', () => {
      const result = parseQuantityAndUnit("bread and butter");
      expect(result).toEqual({
        text: "bread and butter",
        quantity: 1,
        unit: null,
        original: "bread and butter",
      });
    });
  });

  describe("edge cases", () => {
    it("handles empty string", () => {
      const result = parseQuantityAndUnit("");
      expect(result).toEqual({
        text: "",
        quantity: 1,
        unit: null,
        original: "",
      });
    });

    it("handles whitespace only", () => {
      const result = parseQuantityAndUnit("   ");
      expect(result).toEqual({
        text: "",
        quantity: 1,
        unit: null,
        original: "   ",
      });
    });

    it("preserves original input with leading/trailing spaces", () => {
      const result = parseQuantityAndUnit("  2 milk  ");
      expect(result).toEqual({
        text: "milk",
        quantity: 2,
        unit: null,
        original: "  2 milk  ",
      });
    });

    it("handles decimal quantities", () => {
      const result = parseQuantityAndUnit("1.5kg flour");
      expect(result).toEqual({
        text: "flour",
        quantity: 1.5,
        unit: "kg",
        original: "1.5kg flour",
      });
    });

    it("handles very large numbers", () => {
      const result = parseQuantityAndUnit("1000000 items");
      expect(result).toEqual({
        text: "items",
        quantity: 1000000,
        unit: null,
        original: "1000000 items",
      });
    });

    it("handles multi-word item names", () => {
      const result = parseQuantityAndUnit("2 chocolate chip cookies");
      expect(result).toEqual({
        text: "chocolate chip cookies",
        quantity: 2,
        unit: null,
        original: "2 chocolate chip cookies",
      });
    });

    it("handles unit case insensitivity", () => {
      const result = parseQuantityAndUnit("500G cheese");
      expect(result).toEqual({
        text: "cheese",
        quantity: 500,
        unit: "g",
        original: "500G cheese",
      });
    });

    it("handles lbs unit", () => {
      const result = parseQuantityAndUnit("2lbs ground beef");
      expect(result).toEqual({
        text: "ground beef",
        quantity: 2,
        unit: "lbs",
        original: "2lbs ground beef",
      });
    });

    it("does not parse numbers embedded in brand names", () => {
      const result = parseQuantityAndUnit("7up soda");
      expect(result).toEqual({
        text: "7up soda",
        quantity: 1,
        unit: null,
        original: "7up soda",
      });
    });
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

  describe("punctuation removal", () => {
    it('converts "coca-cola" to "cocacola"', () => {
      expect(normalize("coca-cola")).toBe("cocacola");
    });

    it("removes apostrophes", () => {
      expect(normalize("mother's milk")).toBe("mothers milk");
    });

    it("removes periods", () => {
      expect(normalize("Dr. Pepper")).toBe("dr pepper");
    });

    it("removes commas", () => {
      expect(normalize("salt, pepper")).toBe("salt pepper");
    });

    it("removes exclamation marks", () => {
      expect(normalize("Wow!")).toBe("wow");
    });

    it("removes question marks", () => {
      expect(normalize("milk?")).toBe("milk");
    });

    it("removes multiple punctuation types", () => {
      expect(normalize("Hello, World! How's it going?")).toBe(
        "hello world hows it going"
      );
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
      expect(normalize("cafe\u0301")).toBe("cafe");
    });
  });

  describe("combined transformations", () => {
    it("handles all transformations together", () => {
      expect(normalize("  JAMÓN  con  Café! ")).toBe("jamon con cafe");
    });

    it("normalizes for matching purposes", () => {
      // These should all normalize to the same value
      const variants = [
        "Jamón",
        "JAMON",
        "jamón",
        "  jamón  ",
        "JAMÓN!",
        "ja-món",
      ];
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
