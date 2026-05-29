//! Text normalisation and count/size parsing for the matching pipeline.
//!
//! Ported from Shoppy's `src/lib/matching/normalize.ts`, with two deliberate
//! changes mandated by `docs/design.md`:
//!
//! 1. **Punctuation becomes a space, not nothing.** Shoppy strips punctuation
//!    (`"coca-cola"` → `"cocacola"`); BuyWise turns it into a single space
//!    (`"coca-cola"` → `"coca cola"`). A small [brand-compaction table](BRAND_COMPACTIONS)
//!    is applied *before* that step for brands that must compact instead of
//!    split (e.g. `"7-up"` → `"7up"`).
//! 2. **Count vs size are distinguished.** Shoppy returned a single
//!    `{ quantity, unit }`. BuyWise returns [`ParsedInput`] with a `count`
//!    (how many to buy, multiplied by price) *and* an optional
//!    [`RequestedSize`] constraint (a per-pack size, never multiplied). A
//!    number followed by a unit is a size; a bare number / `"2x"` / `"dos"`
//!    is a count.
//!
//! The size parser is also **extended** beyond Shoppy to recognise Spanish
//! long-form units (`"litros"`, `"gramos"`, `"kilos"`, …) and to convert every
//! recognised unit to its SI [`SizeBasis`] (grams or millilitres).

use crate::domain::{ParsedInput, RequestedSize, SizeBasis};
use unicode_normalization::UnicodeNormalization;

/// Brands whose internal punctuation must *compact* rather than split into
/// separate tokens. Applied (as a whole-string, case-insensitive substring
/// replacement) *before* the general punctuation→space rule.
///
/// Keep this table short and explicit. Anything not listed here follows the
/// default rule, so e.g. `"coca-cola"` stays `"coca cola"`.
const BRAND_COMPACTIONS: &[(&str, &str)] = &[
    // The canonical design-doc example: "7-up" is the brand "7up". Only the
    // hyphenated form is compacted: a spaced "7 up" entry would be a substring
    // replace that corrupts unrelated text (e.g. "level 7 update").
    ("7-up", "7up"),
];

/// Normalise text for matching.
///
/// Steps, in order:
/// 1. Lowercase.
/// 2. Apply the [brand-compaction table](BRAND_COMPACTIONS).
/// 3. NFD-decompose and strip combining marks (`U+0300..=U+036F`) → accents
///    are removed (`"jamón"` → `"jamon"`).
/// 4. Replace every character that is neither alphanumeric nor whitespace with
///    a single space (punctuation → space, *not* deletion).
/// 5. Collapse runs of whitespace to one space and trim.
///
/// # Examples
/// ```
/// use buywise_core::normalise::normalize;
/// assert_eq!(normalize("LECHE"), "leche");
/// assert_eq!(normalize("Jamón"), "jamon");
/// assert_eq!(normalize("coca-cola"), "coca cola");
/// assert_eq!(normalize("  multiple   spaces  "), "multiple spaces");
/// ```
pub fn normalize(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // 1. Lowercase.
    let mut lowered = text.to_lowercase();

    // 2. Brand compaction (before punctuation handling). Case-insensitive is
    //    unnecessary now that we have already lowercased.
    for (from, to) in BRAND_COMPACTIONS {
        lowered = lowered.replace(from, to);
    }

    // 3. NFD + strip combining marks. 4. Punctuation → space.
    let mut spaced = String::with_capacity(lowered.len());
    for ch in lowered.nfd() {
        if ('\u{0300}'..='\u{036F}').contains(&ch) {
            // Combining diacritical mark: drop it (accent stripping).
            continue;
        }
        if ch.is_alphanumeric() || ch.is_whitespace() {
            spaced.push(ch);
        } else {
            // Any other character (punctuation, symbols, emoji) → space.
            spaced.push(' ');
        }
    }

    // 5. Collapse whitespace and trim.
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Capitalise the first character of `text`, leaving the rest unchanged.
///
/// Returns `""` for empty input. Non-letter first characters (digits, symbols)
/// are returned unchanged.
///
/// # Examples
/// ```
/// use buywise_core::normalise::to_display_name;
/// assert_eq!(to_display_name("milk"), "Milk");
/// assert_eq!(to_display_name("BREAD"), "BREAD");
/// assert_eq!(to_display_name(""), "");
/// ```
pub fn to_display_name(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Map a (lowercased) unit token to its SI [`SizeBasis`] and a multiplier
/// applied to the parsed value.
///
/// Matches Shoppy's short units and adds the Spanish long-form units required
/// by the design doc:
///
/// * weight → [`SizeBasis::Grams`]: `g`/`gram`/`grams`/`gramo`/`gramos` (×1),
///   `kg`/`kilo`/`kilos`/`kilogramo`/`kilogramos` (×1000)
/// * volume → [`SizeBasis::Millilitres`]: `ml`/`mililitro`/`mililitros` (×1),
///   `cl`/`centilitro`/`centilitros` (×10),
///   `l`/`litro`/`litros`/`litre`/`litres` (×1000)
///
/// Returns `None` for anything else (so imperial units such as `lb`/`oz`, which
/// have no SI [`SizeBasis`] mapping in Phase 0, are *not* treated as sizes).
fn unit_to_basis(unit: &str) -> Option<(SizeBasis, f64)> {
    match unit {
        // Weight → grams.
        "g" | "gram" | "grams" | "gramo" | "gramos" => Some((SizeBasis::Grams, 1.0)),
        "kg" | "kilo" | "kilos" | "kilogramo" | "kilogramos" => Some((SizeBasis::Grams, 1000.0)),
        // Volume → millilitres.
        "ml" | "mililitro" | "mililitros" => Some((SizeBasis::Millilitres, 1.0)),
        "cl" | "centilitro" | "centilitros" => Some((SizeBasis::Millilitres, 10.0)),
        "l" | "litro" | "litros" | "litre" | "litres" => Some((SizeBasis::Millilitres, 1000.0)),
        _ => None,
    }
}

/// Resolve an English or Spanish word-number (`"two"`, `"dos"`, …) in the range
/// 0–12 to its numeric value. Case-insensitive. Returns `None` otherwise.
fn word_number(word: &str) -> Option<u32> {
    match word.to_lowercase().as_str() {
        // English 0–12.
        "zero" => Some(0),
        "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        "ten" => Some(10),
        "eleven" => Some(11),
        "twelve" => Some(12),
        // Spanish 0–12.
        "cero" => Some(0),
        // "un" is the apocopated form used before a unit/noun ("un litro").
        "uno" | "una" | "un" => Some(1),
        "dos" => Some(2),
        "tres" => Some(3),
        "cuatro" => Some(4),
        "cinco" => Some(5),
        "seis" => Some(6),
        "siete" => Some(7),
        "ocho" => Some(8),
        "nueve" => Some(9),
        "diez" => Some(10),
        "once" => Some(11),
        "doce" => Some(12),
        _ => None,
    }
}

/// What a single token resolved to, if anything.
enum Token {
    /// A count (`"2x"`, `"x2"`, a bare number, or a word-number).
    Count(u32),
    /// A size constraint (number immediately followed by a recognised unit).
    Size(RequestedSize),
}

/// Split a token into a leading numeric part and a trailing alphabetic part,
/// stripping a single optional `x` on either side (the `"2x"` / `"x2"` forms).
/// Returns `(number_string, unit_string)` where either may be empty.
fn split_number_unit(token: &str) -> Option<(f64, String)> {
    // Strip a single leading or trailing 'x' (the count marker).
    let core = token
        .strip_prefix('x')
        .or_else(|| token.strip_suffix('x'))
        .unwrap_or(token);

    // The numeric prefix: digits and at most one decimal point. Note: the '.'
    // is only consumed here as part of a number token; a free-standing decimal
    // in raw item text (e.g. the "2.5" in "2.5 milk", which is not a recognised
    // count/size token) is later turned into a space by normalize().
    let mut num = String::new();
    let mut rest_start = 0;
    let mut seen_dot = false;
    for (i, ch) in core.char_indices() {
        if ch.is_ascii_digit() {
            num.push(ch);
            rest_start = i + ch.len_utf8();
        } else if ch == '.' && !seen_dot {
            seen_dot = true;
            num.push(ch);
            rest_start = i + ch.len_utf8();
        } else {
            break;
        }
    }

    if num.is_empty() {
        return None;
    }

    let value: f64 = num.parse().ok()?;
    let unit = core[rest_start..].to_lowercase();
    Some((value, unit))
}

/// Validate a parsed numeric value as a count.
///
/// A count must be a finite, positive (`> 0`) integer that fits in `u32`.
/// Anything else — a fraction, zero, a negative, or a value too large for
/// `u32` — is rejected (returns `None`) so the token is left in the item text
/// and the count defaults to 1, rather than silently saturating at
/// [`u32::MAX`] via an `as` cast.
fn count_from_value(value: f64) -> Option<u32> {
    if !value.is_finite() || value.fract() != 0.0 || value <= 0.0 || value > f64::from(u32::MAX) {
        return None;
    }
    Some(value as u32)
}

/// Try to classify a single token as a count or a size constraint.
///
/// * A bare number, `"2x"`, or `"x2"` → [`Token::Count`].
/// * A number immediately followed by a recognised unit (`"500g"`, `"2l"`) →
///   [`Token::Size`].
/// * A word-number (`"two"`, `"dos"`) → [`Token::Count`].
/// * Anything else → `None`.
fn classify_token(token: &str) -> Option<Token> {
    if let Some(n) = word_number(token) {
        return Some(Token::Count(n));
    }

    let (value, unit) = split_number_unit(token)?;

    if unit.is_empty() {
        // Bare number or "2x"/"x2": a count. Only a finite, positive integer in
        // u32 range is a valid count; anything else (fraction, zero, negative,
        // out-of-range) is left in the item text with count defaulting to 1.
        return count_from_value(value).map(Token::Count);
    }

    // Number + unit: a size constraint, if the unit is recognised.
    let (basis, multiplier) = unit_to_basis(&unit)?;
    Some(Token::Size(RequestedSize {
        value: value * multiplier,
        basis,
    }))
}

/// Try to read a spaced `<number> <unit>` pair, where `num_token` is a digit
/// number (`"500"`, `"2.5"`) or a word-number (`"dos"`) and `unit_token` is a
/// recognised (long- or short-form) unit. Returns the [`RequestedSize`] in SI
/// units, or `None` if either token does not fit.
fn spaced_number_unit(num_token: &str, unit_token: &str) -> Option<RequestedSize> {
    let (basis, multiplier) = unit_to_basis(&unit_token.to_lowercase())?;
    let value =
        word_number(num_token)
            .map(f64::from)
            .or_else(|| match split_number_unit(num_token) {
                Some((value, unit)) if unit.is_empty() => Some(value),
                _ => None,
            })?;
    Some(RequestedSize {
        value: value * multiplier,
        basis,
    })
}

/// Build a [`ParsedInput`] from the chosen count/size token and the residual
/// tokens (everything except the consumed token).
fn build(original: &str, residual: &[&str], token: Option<Token>) -> ParsedInput {
    let mut count = 1;
    let mut requested_size = None;
    match token {
        Some(Token::Count(n)) => count = n,
        Some(Token::Size(size)) => requested_size = Some(size),
        None => {}
    }
    ParsedInput {
        text: normalize(&residual.join(" ")),
        count,
        requested_size,
        original: original.to_string(),
    }
}

/// Parse a free-text shopping-list line into a [`ParsedInput`].
///
/// Distinguishes a **count** (how many packs to buy) from a **size**
/// constraint (the per-pack size). The recognised count/size token is stripped
/// and the remaining text is normalised via [`normalize`]:
///
/// * `"500g rice"` → size `{500 g}`, count 1, text `"rice"`
/// * `"dos litros leche"` → size `{2000 ml}`, count 1, text `"leche"`
/// * `"leche 2L"` → size `{2000 ml}`, count 1, text `"leche"`
/// * `"2x milk"` → count 2, no size, text `"milk"`
/// * `"dos leches"` → count 2, no size, text `"leches"` (not singularised)
/// * `"milk"` → count 1, no size, text `"milk"`
///
/// Follows Shoppy's first-token-then-last-token strategy: the first token is
/// tried; failing that, the last token (only when there are at least two).
///
/// A quantity-only input such as `"500g"` is intentionally allowed to yield an
/// empty `text`: the size is consumed and there is no residual item name.
pub fn parse_count_and_size(input: &str) -> ParsedInput {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return ParsedInput {
            text: String::new(),
            count: 1,
            requested_size: None,
            original: input.to_string(),
        };
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();

    // Special case: a number (word or digit) followed by a *separate* unit
    // token, e.g. "dos litros leche" or "500 gramos jamon" → a size. (The
    // joined form like "500g" is a single token handled by classify_token;
    // this covers the spaced form, common with Spanish long-form units.)
    //
    // Try the *leading* pair (tokens 0..=1), then the *trailing* pair
    // (last two tokens, e.g. "leche 2 litros" / "leche dos litros"). The
    // leading pair takes precedence to mirror the first-then-last strategy.
    if tokens.len() >= 2 {
        if let Some(size) = spaced_number_unit(tokens[0], tokens[1]) {
            return build(input, &tokens[2..], Some(Token::Size(size)));
        }
        let last = tokens.len() - 1;
        if let Some(size) = spaced_number_unit(tokens[last - 1], tokens[last]) {
            return build(input, &tokens[..last - 1], Some(Token::Size(size)));
        }
    }

    // Try the first token.
    if let Some(token) = classify_token(tokens[0]) {
        // A bare word-number / count at the very start with nothing after it is
        // really just an item name (e.g. the line "two"); only treat it as a
        // count when there is residual item text. Sizes are always honoured.
        let is_lone = tokens.len() == 1;
        if !is_lone || matches!(token, Token::Size(_)) {
            return build(input, &tokens[1..], Some(token));
        }
    }

    // Try the last token (only with at least two tokens).
    if tokens.len() >= 2 {
        let last = tokens.len() - 1;
        if let Some(token) = classify_token(tokens[last]) {
            return build(input, &tokens[..last], Some(token));
        }
    }

    // No count/size recognised: default count 1, whole line as text.
    build(input, &tokens, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn size(p: &ParsedInput) -> &RequestedSize {
        p.requested_size
            .as_ref()
            .expect("expected a requested_size")
    }

    // ---- normalize ----------------------------------------------------------

    #[test]
    fn normalize_acceptance_examples() {
        assert_eq!(normalize("LECHE"), "leche");
        assert_eq!(normalize("Jamón"), "jamon");
        assert_eq!(normalize("coca-cola"), "coca cola");
        assert_eq!(normalize("  multiple   spaces  "), "multiple spaces");
    }

    #[test]
    fn normalize_lowercase() {
        assert_eq!(normalize("Milk"), "milk");
        assert_eq!(normalize("MIXED Case TEXT"), "mixed case text");
        assert_eq!(normalize("A"), "a");
    }

    #[test]
    fn normalize_accent_stripping() {
        // Ported from Shoppy (identical behaviour).
        assert_eq!(normalize("café"), "cafe");
        assert_eq!(normalize("crème brûlée"), "creme brulee");
        assert_eq!(normalize("niño"), "nino");
        assert_eq!(normalize("naïve"), "naive");
        assert_eq!(normalize("Ägypten"), "agypten");
        assert_eq!(normalize("Müsli"), "musli");
        assert_eq!(normalize("açaí"), "acai");
        // Already-decomposed input (e + combining acute).
        assert_eq!(normalize("cafe\u{0301}"), "cafe");
    }

    #[test]
    fn normalize_punctuation_to_space_not_deletion() {
        // INTENTIONAL DIFFERENCE from Shoppy: Shoppy deletes punctuation
        // ("coca-cola" -> "cocacola"); BuyWise turns it into a space.
        assert_eq!(normalize("coca-cola"), "coca cola");
        assert_eq!(normalize("mother's milk"), "mother s milk"); // Shoppy: "mothers milk"
        assert_eq!(normalize("Dr. Pepper"), "dr pepper");
        assert_eq!(normalize("salt, pepper"), "salt pepper");
        assert_eq!(normalize("Wow!"), "wow");
        assert_eq!(normalize("milk?"), "milk");
        // Shoppy: "hello world hows it going".
        assert_eq!(
            normalize("Hello, World! How's it going?"),
            "hello world how s it going"
        );
        assert_eq!(normalize("ja-món"), "ja mon"); // Shoppy: "jamon"
    }

    #[test]
    fn normalize_whitespace() {
        assert_eq!(normalize("   milk"), "milk");
        assert_eq!(normalize("milk   "), "milk");
        assert_eq!(normalize("milk\t\tbread"), "milk bread");
        assert_eq!(normalize("milk\n\nbread"), "milk bread");
        assert_eq!(normalize("  milk \t bread \n eggs  "), "milk bread eggs");
    }

    #[test]
    fn normalize_edge_cases() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
        assert_eq!(normalize("7Up"), "7up");
        assert_eq!(normalize("Vitamin B12"), "vitamin b12");
        // Emoji are non-alphanumeric → space → collapsed away.
        assert_eq!(normalize("milk 🥛"), "milk");
        assert_eq!(normalize("🍎 apple 🍏"), "apple");
        assert_eq!(normalize("🥛🍞🥚"), "");
        // Non-Latin scripts are alphanumeric and survive unchanged.
        assert_eq!(normalize("Молоко"), "молоко");
        assert_eq!(normalize("牛奶"), "牛奶");
        assert_eq!(normalize("ミルク"), "ミルク");
    }

    #[test]
    fn normalize_combined() {
        assert_eq!(normalize("  JAMÓN  con  Café! "), "jamon con cafe");
        // All variants normalise to the same value (one exception: "ja-món"
        // becomes "ja mon" here, unlike Shoppy's "jamon").
        let variants = ["Jamón", "JAMON", "jamón", "  jamón  ", "JAMÓN!"];
        let normalized: Vec<String> = variants.iter().map(|v| normalize(v)).collect();
        assert!(normalized.iter().all(|n| n == "jamon"));
    }

    #[test]
    fn normalize_brand_compaction() {
        // Brand-compaction table: only the hyphenated "7-up" compacts to "7up".
        assert_eq!(normalize("7-up"), "7up");
        assert_eq!(normalize("7-Up Zero"), "7up zero");
        // Not in the table → default punctuation→space rule.
        assert_eq!(normalize("coca-cola"), "coca cola");
        assert_eq!(normalize("pepsi-max"), "pepsi max");
    }

    #[test]
    fn normalize_brand_compaction_does_not_over_reach() {
        // A spaced "7 up" entry would substring-replace into unrelated text;
        // it was removed. These must NOT be compacted.
        assert_eq!(normalize("level 7 update"), "level 7 update");
        assert_eq!(normalize("lot 7 upgrade"), "lot 7 upgrade");
        // The hyphenated brand form still compacts.
        assert_eq!(normalize("7-up"), "7up");
    }

    // ---- to_display_name ----------------------------------------------------

    #[test]
    fn display_name_cases() {
        assert_eq!(to_display_name("milk"), "Milk");
        assert_eq!(to_display_name("Milk"), "Milk");
        assert_eq!(to_display_name("mILK"), "MILK");
        assert_eq!(to_display_name("BREAD"), "BREAD");
        assert_eq!(to_display_name("a"), "A");
        assert_eq!(to_display_name(""), "");
        assert_eq!(to_display_name("whole wheat bread"), "Whole wheat bread");
        assert_eq!(to_display_name("éclair"), "Éclair");
        assert_eq!(to_display_name("7up"), "7up");
        assert_eq!(to_display_name("@milk"), "@milk");
    }

    // ---- parse_count_and_size: acceptance criteria --------------------------

    #[test]
    fn parse_acceptance_examples() {
        let p = parse_count_and_size("dos litros leche");
        assert_eq!(p.text, "leche");
        assert_eq!(p.count, 1);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);
        assert_eq!(size(&p).value, 2000.0);

        let p = parse_count_and_size("500g rice");
        assert_eq!(p.text, "rice");
        assert_eq!(p.count, 1);
        assert_eq!(size(&p).basis, SizeBasis::Grams);
        assert_eq!(size(&p).value, 500.0);

        let p = parse_count_and_size("2x milk");
        assert_eq!(p.text, "milk");
        assert_eq!(p.count, 2);
        assert!(p.requested_size.is_none());

        let p = parse_count_and_size("milk");
        assert_eq!(p.text, "milk");
        assert_eq!(p.count, 1);
        assert!(p.requested_size.is_none());
    }

    // ---- parse_count_and_size: count forms (ported from Shoppy) -------------

    #[test]
    fn parse_count_x_forms() {
        // Shoppy returned quantity here; BuyWise returns count (no size).
        for (input, text) in [
            ("2x milk", "milk"),
            ("milk x2", "milk"),
            ("3x apples", "apples"),
        ] {
            let p = parse_count_and_size(input);
            assert_eq!(p.text, text, "input {input}");
            assert!(p.requested_size.is_none(), "input {input}");
            assert_eq!(p.original, input);
        }
        assert_eq!(parse_count_and_size("2x milk").count, 2);
        assert_eq!(parse_count_and_size("milk x2").count, 2);
        assert_eq!(parse_count_and_size("3x apples").count, 3);
    }

    #[test]
    fn parse_count_bare_number() {
        assert_eq!(parse_count_and_size("2 milk").count, 2);
        assert_eq!(parse_count_and_size("2 milk").text, "milk");
        assert_eq!(parse_count_and_size("milk 2").count, 2);
        assert_eq!(parse_count_and_size("milk 2").text, "milk");
        assert_eq!(parse_count_and_size("5 bananas").count, 5);
        assert_eq!(parse_count_and_size("1000000 items").count, 1_000_000);
        let p = parse_count_and_size("2 chocolate chip cookies");
        assert_eq!(p.count, 2);
        assert_eq!(p.text, "chocolate chip cookies");
    }

    #[test]
    fn parse_count_english_word_numbers() {
        let p = parse_count_and_size("two eggs");
        assert_eq!((p.count, p.text.as_str()), (2, "eggs"));
        assert_eq!(parse_count_and_size("three apples").count, 3);
        assert_eq!(parse_count_and_size("twelve donuts").count, 12);
        // Case-insensitive.
        assert_eq!(parse_count_and_size("TWO eggs").count, 2);
        // Full 0-12 sweep.
        for (w, n) in [
            ("zero", 0u32),
            ("one", 1),
            ("two", 2),
            ("three", 3),
            ("four", 4),
            ("five", 5),
            ("six", 6),
            ("seven", 7),
            ("eight", 8),
            ("nine", 9),
            ("ten", 10),
            ("eleven", 11),
            ("twelve", 12),
        ] {
            let p = parse_count_and_size(&format!("{w} things"));
            assert_eq!(p.count, n, "word {w}");
            assert_eq!(p.text, "things", "word {w}");
        }
    }

    #[test]
    fn parse_count_spanish_word_numbers() {
        // "dos leches" -> count 2, text "leches" (NOT singularised).
        let p = parse_count_and_size("dos leches");
        assert_eq!(p.count, 2);
        assert_eq!(p.text, "leches");

        assert_eq!(parse_count_and_size("dos huevos").count, 2);
        assert_eq!(parse_count_and_size("tres manzanas").count, 3);
        assert_eq!(parse_count_and_size("una botella").count, 1);
        assert_eq!(parse_count_and_size("diez naranjas").count, 10);

        for (w, n) in [
            ("cero", 0u32),
            ("uno", 1),
            ("una", 1),
            ("dos", 2),
            ("tres", 3),
            ("cuatro", 4),
            ("cinco", 5),
            ("seis", 6),
            ("siete", 7),
            ("ocho", 8),
            ("nueve", 9),
            ("diez", 10),
            ("once", 11),
            ("doce", 12),
        ] {
            let p = parse_count_and_size(&format!("{w} cosas"));
            assert_eq!(p.count, n, "word {w}");
            assert_eq!(p.text, "cosas", "word {w}");
        }
    }

    // ---- parse_count_and_size: size forms -----------------------------------

    #[test]
    fn parse_size_short_units() {
        let p = parse_count_and_size("500g cheese");
        assert_eq!(p.text, "cheese");
        assert_eq!(p.count, 1);
        assert_eq!(size(&p).value, 500.0);
        assert_eq!(size(&p).basis, SizeBasis::Grams);

        // Last-token form.
        let p = parse_count_and_size("cheese 500g");
        assert_eq!(p.text, "cheese");
        assert_eq!(size(&p).value, 500.0);

        // kg -> grams x1000.
        let p = parse_count_and_size("1kg rice");
        assert_eq!(p.text, "rice");
        assert_eq!(size(&p).value, 1000.0);
        assert_eq!(size(&p).basis, SizeBasis::Grams);

        // ml.
        let p = parse_count_and_size("250ml milk");
        assert_eq!(size(&p).value, 250.0);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);

        // l -> ml x1000 (and "leche 2L" last-token form).
        let p = parse_count_and_size("2l water");
        assert_eq!(p.text, "water");
        assert_eq!(size(&p).value, 2000.0);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);

        let p = parse_count_and_size("leche 2L");
        assert_eq!(p.text, "leche");
        assert_eq!(size(&p).value, 2000.0);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);

        // cl -> ml x10.
        let p = parse_count_and_size("33cl coke");
        assert!((size(&p).value - 330.0).abs() < EPS);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);

        // Unit case-insensitivity.
        let p = parse_count_and_size("500G cheese");
        assert_eq!(size(&p).value, 500.0);

        // Decimal value.
        let p = parse_count_and_size("1.5kg flour");
        assert_eq!(p.text, "flour");
        assert!((size(&p).value - 1500.0).abs() < EPS);
    }

    #[test]
    fn parse_size_spanish_long_form_units() {
        // The extension over Shoppy: Spanish long-form units.
        let cases = [
            ("dos litros leche", 2000.0, SizeBasis::Millilitres, "leche"),
            ("un litro agua", 1000.0, SizeBasis::Millilitres, "agua"),
            ("500 gramos jamon", 500.0, SizeBasis::Grams, "jamon"),
            ("dos kilos patatas", 2000.0, SizeBasis::Grams, "patatas"),
            ("250 mililitros nata", 250.0, SizeBasis::Millilitres, "nata"),
            ("50 centilitros zumo", 500.0, SizeBasis::Millilitres, "zumo"),
            ("un kilogramo harina", 1000.0, SizeBasis::Grams, "harina"),
            ("100 gramo cafe", 100.0, SizeBasis::Grams, "cafe"),
        ];
        for (input, value, basis, text) in cases {
            let p = parse_count_and_size(input);
            assert_eq!(p.text, text, "input {input}");
            assert_eq!(p.count, 1, "input {input}");
            assert!((size(&p).value - value).abs() < EPS, "input {input}");
            assert_eq!(size(&p).basis, basis, "input {input}");
        }
    }

    #[test]
    fn parse_word_number_plus_long_unit() {
        // "dos litros" — number is a word, unit is a separate token.
        let p = parse_count_and_size("dos litros leche");
        assert_eq!(p.count, 1);
        assert_eq!(size(&p).value, 2000.0);
        assert_eq!(p.text, "leche");
    }

    #[test]
    fn parse_trailing_spaced_number_plus_long_unit() {
        // M1: a trailing "<number> <long-unit>" pair, mirroring the leading
        // path. Previously "leche 2 litros" yielded no size.
        let p = parse_count_and_size("leche 2 litros");
        assert_eq!(p.text, "leche");
        assert_eq!(p.count, 1);
        assert_eq!(size(&p).value, 2000.0);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);

        // Trailing word-number pair.
        let p = parse_count_and_size("leche dos litros");
        assert_eq!(p.text, "leche");
        assert_eq!(p.count, 1);
        assert_eq!(size(&p).value, 2000.0);
        assert_eq!(size(&p).basis, SizeBasis::Millilitres);

        // Trailing digit + grams.
        let p = parse_count_and_size("jamon 500 gramos");
        assert_eq!(p.text, "jamon");
        assert_eq!(size(&p).value, 500.0);
        assert_eq!(size(&p).basis, SizeBasis::Grams);

        // Leading pair still takes precedence when both could match.
        let p = parse_count_and_size("dos litros leche");
        assert_eq!(p.text, "leche");
        assert_eq!(size(&p).value, 2000.0);
    }

    #[test]
    fn parse_count_bounds_check() {
        // M2: a value too large for u32 must not silently saturate; the token
        // stays in the item text and count defaults to 1.
        let p = parse_count_and_size("999999999999999 items");
        assert_eq!(p.count, 1);
        assert_eq!(p.text, "999999999999999 items");
        assert!(p.requested_size.is_none());

        // Zero is not a meaningful count → not consumed, count defaults to 1.
        let p = parse_count_and_size("0 milk");
        assert_eq!(p.count, 1);
        assert_eq!(p.text, "0 milk");
        assert!(p.requested_size.is_none());

        // The boundary value u32::MAX is still a valid count.
        let p = parse_count_and_size(&format!("{} items", u32::MAX));
        assert_eq!(p.count, u32::MAX);
        assert_eq!(p.text, "items");
    }

    // ---- parse_count_and_size: edge cases -----------------------------------

    #[test]
    fn parse_no_quantity_defaults_to_one() {
        let p = parse_count_and_size("milk");
        assert_eq!((p.count, p.text.as_str()), (1, "milk"));
        assert!(p.requested_size.is_none());

        let p = parse_count_and_size("bread and butter");
        assert_eq!(p.count, 1);
        assert_eq!(p.text, "bread and butter");
    }

    #[test]
    fn parse_empty_and_whitespace() {
        let p = parse_count_and_size("");
        assert_eq!((p.count, p.text.as_str(), p.original.as_str()), (1, "", ""));
        assert!(p.requested_size.is_none());

        let p = parse_count_and_size("   ");
        assert_eq!(p.count, 1);
        assert_eq!(p.text, "");
        assert_eq!(p.original, "   ");
    }

    #[test]
    fn parse_preserves_original_with_surrounding_spaces() {
        let p = parse_count_and_size("  2 milk  ");
        assert_eq!(p.text, "milk");
        assert_eq!(p.count, 2);
        assert_eq!(p.original, "  2 milk  ");
    }

    #[test]
    fn parse_does_not_split_brand_numbers() {
        // "7up soda": "7up" is not a number+unit (no unit) nor a bare number,
        // so it is left as item text; count defaults to 1.
        let p = parse_count_and_size("7up soda");
        assert_eq!(p.count, 1);
        assert!(p.requested_size.is_none());
        assert_eq!(p.text, "7up soda");
    }

    #[test]
    fn parse_imperial_units_are_not_sizes() {
        // INTENTIONAL DIFFERENCE: Shoppy recognised lb/oz/pt/etc. as units.
        // BuyWise's SizeBasis only covers SI weight/volume, so an unknown unit
        // like "lb" is not a size. "1lb" is a single token with a non-empty,
        // unrecognised unit, so it is not a count either: the token is left in
        // the item text and count defaults to 1.
        let p = parse_count_and_size("1lb butter");
        assert!(p.requested_size.is_none());
        assert_eq!(p.count, 1);
        assert_eq!(p.text, "1lb butter");
    }

    #[test]
    fn parse_text_is_normalised() {
        // Residual item text is run through normalize().
        let p = parse_count_and_size("2 Coca-Cola");
        assert_eq!(p.count, 2);
        assert_eq!(p.text, "coca cola");

        let p = parse_count_and_size("dos Jamón");
        assert_eq!(p.count, 2);
        assert_eq!(p.text, "jamon");
    }
}
