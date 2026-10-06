// Euro-string ↔ integer-cents helpers for the Purchases screen (issue #156).
//
// Money is integer cents on the wire and in storage; this is the one place a
// typed euro amount becomes cents. A comma or a dot is accepted as the decimal
// separator — an Irish/EU keyboard types the comma, a phone keypad the dot — and
// the € sign and grouping spaces are ignored. Thousands separators are NOT
// parsed, because "1.234" is genuinely ambiguous between €1.23 and €1234; an
// amount with more than one separator is refused rather than guessed.
//
// The conversion is integer arithmetic end to end (whole euros × 100 plus the
// first two decimal digits, rounded on the third), so no binary floating point
// ever touches a stored amount.

/**
 * Integer cents for a typed euro amount, or `null` when the text is not a valid
 * amount. A leading `-` yields a negative value; whether that is allowed is the
 * caller's decision (a receipt total is not, a line discount is).
 */
export function parseEurosToCents(raw: string): number | null {
	const cleaned = raw
		.trim()
		.replace(/\s+/g, '')
		.replace(/€/g, '')
		.replace(',', '.');
	if (cleaned === '') return null;

	const match = /^(-?)(\d*)(?:\.(\d*))?$/.exec(cleaned);
	if (!match) return null;
	const [, sign, whole = '', fraction = ''] = match;
	if (whole === '' && fraction === '') return null;

	// Three decimals so the third digit can decide the rounding (half away from
	// zero), matching how a printed amount rounds to the nearest cent.
	const padded = `${fraction}000`.slice(0, 3);
	let cents = Number(whole || '0') * 100 + Number(padded.slice(0, 2));
	if (Number(padded[2]) >= 5) cents += 1;
	return sign === '-' ? -cents : cents;
}

/**
 * A plain euro string for a form input — no symbol, two decimals — e.g.
 * `1234` → `"12.34"`. The inverse of {@link parseEurosToCents} for a form field.
 */
export function toEurosInput(cents: number): string {
	const sign = cents < 0 ? '-' : '';
	const abs = Math.abs(cents);
	return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, '0')}`;
}
