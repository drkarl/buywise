export interface ParsedInput {
  text: string;
  quantity: number;
  unit: string | null;
  original: string;
}

/**
 * Normalizes text for matching by:
 * 1. Converting to lowercase
 * 2. Removing accents (NFD normalization + strip combining marks)
 * 3. Removing punctuation (keeps alphanumeric and spaces)
 * 4. Collapsing multiple whitespace to single space
 * 5. Trimming leading/trailing whitespace
 *
 * @example
 * normalize("LECHE") // "leche"
 * normalize("Jamón") // "jamon"
 * normalize("coca-cola") // "cocacola"
 * normalize("  multiple   spaces  ") // "multiple spaces"
 */
export function normalize(text: string): string {
  // Defensive guard for null/undefined when called from JS or through type escapes
  if (!text) return "";
  return (
    text
      // 1. Convert to lowercase
      .toLowerCase()
      // 2. NFD normalization decomposes accented chars (é → e + combining acute)
      //    Then remove combining diacritical marks (Unicode range \u0300-\u036f)
      .normalize("NFD")
      .replace(/[\u0300-\u036f]/g, "")
      // 3. Remove punctuation (keep only alphanumeric and whitespace)
      .replace(/[^\p{L}\p{N}\s]/gu, "")
      // 4. Collapse multiple whitespace to single space
      .replace(/\s+/g, " ")
      // 5. Trim
      .trim()
  );
}

/**
 * Formats text for display by capitalizing the first letter.
 *
 * @example
 * toDisplayName("milk") // "Milk"
 * toDisplayName("BREAD") // "BREAD" (only capitalizes first, doesn't lowercase rest)
 * toDisplayName("") // ""
 */
export function toDisplayName(text: string): string {
  if (!text) return text;
  return text.charAt(0).toUpperCase() + text.slice(1);
}

const WORD_NUMBERS: Record<string, number> = {
  // English
  zero: 0,
  one: 1,
  two: 2,
  three: 3,
  four: 4,
  five: 5,
  six: 6,
  seven: 7,
  eight: 8,
  nine: 9,
  ten: 10,
  eleven: 11,
  twelve: 12,
  // Spanish
  cero: 0,
  uno: 1,
  una: 1,
  dos: 2,
  tres: 3,
  cuatro: 4,
  cinco: 5,
  seis: 6,
  siete: 7,
  ocho: 8,
  nueve: 9,
  diez: 10,
  once: 11,
  doce: 12,
};

const UNITS = [
  "g",
  "kg",
  "ml",
  "l",
  "lb",
  "lbs",
  "oz",
  "pt",
  "qt",
  "gal",
  "cup",
  "cups",
  "tbsp",
  "tsp",
];

const UNIT_PATTERN = UNITS.join("|");

// Patterns for quantity with optional unit
// Examples:
//   "2x"     -> quantity: 2, unit: null
//   "x2"     -> quantity: 2, unit: null
//   "500g"   -> quantity: 500, unit: "g"
//   "1.5kg"  -> quantity: 1.5, unit: "kg"
//   "2"      -> quantity: 2, unit: null
const QUANTITY_UNIT_REGEX = new RegExp(
  `^(\\d+(?:\\.\\d+)?)(${UNIT_PATTERN})?x?$|^x?(\\d+(?:\\.\\d+)?)(${UNIT_PATTERN})?$`,
  "i"
);

function parseQuantityUnit(token: string): {
  quantity: number;
  unit: string | null;
} | null {
  const match = token.match(QUANTITY_UNIT_REGEX);
  if (!match) return null;

  // Groups 1,2 for leading number, groups 3,4 for trailing number (after x)
  const numStr = match[1] || match[3];
  const unitStr = match[2] || match[4];

  if (!numStr) return null;

  const quantity = parseFloat(numStr);
  if (!Number.isFinite(quantity)) return null;

  return {
    quantity,
    unit: unitStr ? unitStr.toLowerCase() : null,
  };
}

function parseWordNumber(word: string): number | null {
  const normalized = word.toLowerCase();
  return WORD_NUMBERS[normalized] ?? null;
}

export function parseQuantityAndUnit(input: string): ParsedInput {
  const original = input;
  const trimmed = input.trim();

  if (!trimmed) {
    return { text: "", quantity: 1, unit: null, original };
  }

  const tokens = trimmed.split(/\s+/);

  // Try parsing first token as quantity/unit
  if (tokens.length >= 1) {
    const firstParsed = parseQuantityUnit(tokens[0]);
    if (firstParsed) {
      const text = tokens.slice(1).join(" ");
      return {
        text,
        quantity: firstParsed.quantity,
        unit: firstParsed.unit,
        original,
      };
    }

    // Check for word number at start
    const firstWordNum = parseWordNumber(tokens[0]);
    if (firstWordNum !== null && tokens.length > 1) {
      const text = tokens.slice(1).join(" ");
      return {
        text,
        quantity: firstWordNum,
        unit: null,
        original,
      };
    }
  }

  // Try parsing last token as quantity/unit
  if (tokens.length >= 2) {
    const lastParsed = parseQuantityUnit(tokens[tokens.length - 1]);
    if (lastParsed) {
      const text = tokens.slice(0, -1).join(" ");
      return {
        text,
        quantity: lastParsed.quantity,
        unit: lastParsed.unit,
        original,
      };
    }

    // Check for word number at end
    const lastWordNum = parseWordNumber(tokens[tokens.length - 1]);
    if (lastWordNum !== null) {
      const text = tokens.slice(0, -1).join(" ");
      return {
        text,
        quantity: lastWordNum,
        unit: null,
        original,
      };
    }
  }

  // No quantity found, default to 1
  return {
    text: trimmed,
    quantity: 1,
    unit: null,
    original,
  };
}
