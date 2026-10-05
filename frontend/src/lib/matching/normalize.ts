/**
 * A per-pack size constraint normalised to a base unit: grams for weight,
 * millilitres for volume. Mirrors `RequestedSize` in `crates/core/src/normalise.rs`.
 */
export interface RequestedSize {
  /** Magnitude in the base unit named by `basis`. */
  value: number;
  /** Base unit: `"g"` (weight) or `"ml"` (volume). */
  basis: "g" | "ml";
}

export interface ParsedInput {
  /** Item text with any recognised count/size token stripped, then normalised. */
  text: string;
  /** Pack count (how many to buy); 1 when no count token is present. */
  count: number;
  /** Per-pack size constraint in base units, or `null` when none is requested. */
  requestedSize: RequestedSize | null;
  /** The original, untrimmed input. */
  original: string;
}

/**
 * Brands whose internal punctuation must *compact* rather than split into
 * separate tokens. Applied as a whole-string substring replacement *before* the
 * general punctuation→space rule.
 *
 * Mirrors `BRAND_COMPACTIONS` in `crates/core/src/normalise.rs` — keep this
 * table short, explicit, and in sync with the Rust core. Only the hyphenated
 * "7-up" form is compacted: a spaced "7 up" entry would corrupt unrelated text
 * (e.g. "level 7 update").
 */
const BRAND_COMPACTIONS: ReadonlyArray<readonly [string, string]> = [
  ["7-up", "7up"],
];

/**
 * Normalizes text for matching. Mirrors the authoritative core normaliser in
 * `crates/core/src/normalise.rs` exactly. Steps, in order:
 * 1. Lowercase.
 * 2. Apply the brand-compaction table (BRAND_COMPACTIONS) before punctuation
 *    handling.
 * 3. NFD normalization + strip combining marks (accents removed).
 * 4. Punctuation → single SPACE (every char that is neither alphanumeric nor
 *    whitespace), NOT deletion.
 * 5. Collapse multiple whitespace to a single space + trim.
 *
 * @example
 * normalize("LECHE") // "leche"
 * normalize("Jamón") // "jamon"
 * normalize("coca-cola") // "coca cola"
 * normalize("7-up") // "7up"
 * normalize("  multiple   spaces  ") // "multiple spaces"
 */
export function normalize(text: string): string {
  // Defensive guard for null/undefined when called from JS or through type escapes
  if (!text) return "";

  // 1. Convert to lowercase.
  let lowered = text.toLowerCase();

  // 2. Brand compaction (before punctuation handling). Already lowercased, so a
  //    plain substring replace is effectively case-insensitive.
  for (const [from, to] of BRAND_COMPACTIONS) {
    lowered = lowered.split(from).join(to);
  }

  return (
    lowered
      // 3. NFD normalization decomposes accented chars (é → e + combining acute)
      //    Then remove combining diacritical marks (Unicode range \u0300-\u036f)
      .normalize("NFD")
      .replace(/[\u0300-\u036f]/g, "")
      // 4. Punctuation → single space (replace every non-alphanumeric,
      //    non-whitespace char with a space, NOT deletion).
      .replace(/[^\p{L}\p{N}\s]/gu, " ")
      // 5. Collapse multiple whitespace to single space ...
      .replace(/\s+/g, " ")
      // ... and trim.
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

// ---------------------------------------------------------------------------
// Count / size parsing.
//
// Faithfully ports `parse_count_and_size` (and its helpers) from
// `crates/core/src/normalise.rs` — the authoritative contract — so the
// client-side duplicate check agrees with the backend. Two key differences from
// the original Shoppy parser this replaces:
//   1. Size units are SI weight/volume ONLY, with English + Spanish long forms,
//      converted to a base unit (g / ml). Imperial units (lb, oz, pt, cup, …)
//      are NOT sizes — a number followed by one stays in the item text.
//   2. Spaced "<number> <unit>" pairs are recognised ("dos litros leche",
//      "500 gramos jamon"), not just the joined form ("500g").
// ---------------------------------------------------------------------------

/**
 * English/Spanish word-numbers 0–12, including the apocopated Spanish `un`
 * ("un litro"). Mirrors `word_number` in the Rust core.
 */
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
  un: 1,
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

function wordNumber(word: string): number | null {
  return WORD_NUMBERS[word.toLowerCase()] ?? null;
}

/**
 * Map a lowercased unit token to its base unit and a multiplier. Mirrors
 * `unit_to_basis`: SI weight (→ grams) and volume (→ millilitres) only, with
 * English and Spanish long forms. Imperial units (`lb`, `oz`, `pt`, …) have no
 * base mapping and return `null`, so they are never treated as sizes.
 */
function unitToBasis(unit: string): [RequestedSize["basis"], number] | null {
  switch (unit) {
    // Weight → grams.
    case "g":
    case "gram":
    case "grams":
    case "gramo":
    case "gramos":
      return ["g", 1];
    case "kg":
    case "kilo":
    case "kilos":
    case "kilogramo":
    case "kilogramos":
      return ["g", 1000];
    // Volume → millilitres.
    case "ml":
    case "mililitro":
    case "mililitros":
      return ["ml", 1];
    case "cl":
    case "centilitro":
    case "centilitros":
      return ["ml", 10];
    case "l":
    case "litro":
    case "litros":
    case "litre":
    case "litres":
      return ["ml", 1000];
    default:
      return null;
  }
}

/**
 * Split a token into a leading numeric value and a trailing unit string,
 * stripping a single optional `x` on either side (the `2x` / `x2` count forms).
 * Mirrors `split_number_unit`. Returns `null` when there is no numeric prefix.
 */
function splitNumberUnit(token: string): { value: number; unit: string } | null {
  // Strip a single leading OR trailing 'x' (the count marker; prefix wins).
  let core = token;
  if (core.startsWith("x")) core = core.slice(1);
  else if (core.endsWith("x")) core = core.slice(0, -1);

  // Numeric prefix: digits and at most one decimal point.
  let num = "";
  let restStart = 0;
  let seenDot = false;
  for (let i = 0; i < core.length; i++) {
    const ch = core[i];
    if (ch >= "0" && ch <= "9") {
      num += ch;
      restStart = i + 1;
    } else if (ch === "." && !seenDot) {
      seenDot = true;
      num += ch;
      restStart = i + 1;
    } else {
      break;
    }
  }

  if (num === "") return null;
  const value = Number.parseFloat(num);
  if (!Number.isFinite(value)) return null;
  return { value, unit: core.slice(restStart).toLowerCase() };
}

/**
 * Validate a numeric value as a count: a finite, positive integer in `u32`
 * range. Fractions, zero, negatives, and out-of-range values are rejected (the
 * token is left in the item text and the count defaults to 1). Mirrors
 * `count_from_value`.
 */
function countFromValue(value: number): number | null {
  if (
    !Number.isFinite(value) ||
    !Number.isInteger(value) ||
    value <= 0 ||
    value > 4_294_967_295
  ) {
    return null;
  }
  return value;
}

type Token =
  | { kind: "count"; count: number }
  | { kind: "size"; size: RequestedSize };

/**
 * Classify a single token as a count or a size, or `null`. Mirrors
 * `classify_token`: a word-number, bare number, or `2x`/`x2` → count; a number
 * immediately followed by a recognised SI unit → size; anything else → `null`.
 */
function classifyToken(token: string): Token | null {
  const wn = wordNumber(token);
  if (wn !== null) return { kind: "count", count: wn };

  const split = splitNumberUnit(token);
  if (!split) return null;

  if (split.unit === "") {
    const count = countFromValue(split.value);
    return count === null ? null : { kind: "count", count };
  }

  const basis = unitToBasis(split.unit);
  if (!basis) return null;
  return { kind: "size", size: { value: split.value * basis[1], basis: basis[0] } };
}

/**
 * Read a spaced `<number> <unit>` pair (e.g. `dos litros`, `500 gramos`) into a
 * size, or `null`. Mirrors `spaced_number_unit`: the number may be a digit or a
 * word-number, and the unit must be a recognised SI unit.
 */
function spacedNumberUnit(numToken: string, unitToken: string): RequestedSize | null {
  const basis = unitToBasis(unitToken.toLowerCase());
  if (!basis) return null;

  let value = wordNumber(numToken);
  if (value === null) {
    const split = splitNumberUnit(numToken);
    if (split && split.unit === "") value = split.value;
    else return null;
  }
  return { value: value * basis[1], basis: basis[0] };
}

/** A whitespace-delimited token with its character range inside the trimmed input. */
interface OffsetToken {
  value: string;
  start: number;
  end: number;
}

/** Tokenise `trimmed` into non-whitespace runs, recording each one's char range. */
function tokenize(trimmed: string): OffsetToken[] {
  const tokens: OffsetToken[] = [];
  const re = /\S+/g;
  let match: RegExpExecArray | null;
  while ((match = re.exec(trimmed)) !== null) {
    tokens.push({ value: match[0], start: match.index, end: match.index + match[0].length });
  }
  return tokens;
}

/** Build a {@link ParsedInput} from the chosen token and residual token values. */
function build(original: string, residual: string[], token: Token | null): ParsedInput {
  let count = 1;
  let requestedSize: RequestedSize | null = null;
  if (token?.kind === "count") count = token.count;
  else if (token?.kind === "size") requestedSize = token.size;
  return { text: normalize(residual.join(" ")), count, requestedSize, original };
}

/**
 * A {@link ParsedInput} plus the character ranges of the tokens the parser
 * consumed, relative to `input.trim()`. `countSpan` / `sizeSpan` are `null` when
 * no token of that kind was stripped. Offsets index the TRIMMED input, so callers
 * slice `input.trim()`, not the raw string.
 */
export interface ParsedInputSpans extends ParsedInput {
  countSpan: [number, number] | null;
  sizeSpan: [number, number] | null;
}

/** Shared implementation behind the two public parse entry points. */
function parseWithSpans(input: string): ParsedInputSpans {
  const trimmed = input.trim();
  if (trimmed === "") {
    return {
      text: "",
      count: 1,
      requestedSize: null,
      original: input,
      countSpan: null,
      sizeSpan: null,
    };
  }

  const tokens = tokenize(trimmed);

  // Spaced "<number> <unit>" pairs; the leading pair takes precedence over the
  // trailing pair (mirrors the first-then-last strategy).
  if (tokens.length >= 2) {
    const leading = spacedNumberUnit(tokens[0].value, tokens[1].value);
    if (leading) {
      return {
        ...build(
          input,
          tokens.slice(2).map((t) => t.value),
          { kind: "size", size: leading }
        ),
        countSpan: null,
        sizeSpan: [tokens[0].start, tokens[1].end],
      };
    }

    const last = tokens.length - 1;
    const trailing = spacedNumberUnit(tokens[last - 1].value, tokens[last].value);
    if (trailing) {
      return {
        ...build(
          input,
          tokens.slice(0, last - 1).map((t) => t.value),
          { kind: "size", size: trailing }
        ),
        countSpan: null,
        sizeSpan: [tokens[last - 1].start, tokens[last].end],
      };
    }
  }

  // First token. A lone count is really an item name ("two", "2"); only strip a
  // leading count when residual text follows. A lone size is always honoured.
  const first = classifyToken(tokens[0].value);
  if (first && (tokens.length > 1 || first.kind === "size")) {
    return {
      ...build(
        input,
        tokens.slice(1).map((t) => t.value),
        first
      ),
      countSpan: first.kind === "count" ? [tokens[0].start, tokens[0].end] : null,
      sizeSpan: first.kind === "size" ? [tokens[0].start, tokens[0].end] : null,
    };
  }

  // Last token (only with at least two tokens).
  if (tokens.length >= 2) {
    const last = tokens.length - 1;
    const lastToken = classifyToken(tokens[last].value);
    if (lastToken) {
      return {
        ...build(
          input,
          tokens.slice(0, last).map((t) => t.value),
          lastToken
        ),
        countSpan:
          lastToken.kind === "count" ? [tokens[last].start, tokens[last].end] : null,
        sizeSpan:
          lastToken.kind === "size" ? [tokens[last].start, tokens[last].end] : null,
      };
    }
  }

  // No count/size recognised: the whole line is text, count 1.
  return {
    ...build(
      input,
      tokens.map((t) => t.value),
      null
    ),
    countSpan: null,
    sizeSpan: null,
  };
}

/**
 * Parse a free-text shopping-list line into a {@link ParsedInput}, distinguishing
 * a COUNT (how many packs) from a per-pack SIZE constraint. Faithfully mirrors
 * `parse_count_and_size` in `crates/core/src/normalise.rs`:
 *
 * - `"500g rice"` / `"500 g rice"` → size `{500 g}`, text `"rice"`
 * - `"dos litros leche"` / `"2l leche"` → size `{2000 ml}`, text `"leche"`
 * - `"2x milk"` → count 2, no size, text `"milk"`
 * - `"1lb butter"` → no size (imperial), text `"1lb butter"`
 *
 * Strategy: leading spaced pair, then trailing spaced pair, then first token,
 * then last token; otherwise the whole line is text. A lone count token is left
 * in the text (only a leading count with residual text is stripped); a lone size
 * is always honoured (`"500g"` → text `""`).
 */
export function parseQuantityAndUnit(input: string): ParsedInput {
  // Strip the span bookkeeping so the returned object is exactly `ParsedInput`
  // (callers/tests pin the shape with `toEqual`).
  const { countSpan: _countSpan, sizeSpan: _sizeSpan, ...parsed } = parseWithSpans(input);
  return parsed;
}

/**
 * {@link parseQuantityAndUnit} plus the character ranges of the count/size tokens
 * the parser stripped. The bulk-add preview uses `countSpan` to lift a leading or
 * trailing count ("2x eggs") into a separate editable quantity while leaving a
 * size ("500g mince") in the item text — the shared parser stays the single
 * source of truth for what counts as a count or a size.
 */
export function parseQuantityAndUnitSpans(input: string): ParsedInputSpans {
  return parseWithSpans(input);
}
