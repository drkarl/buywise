// Client types + helper for the savings dashboard (P5-T4, relabelled in #117).
//
// These mirror the wire shape of `GET /api/savings/summary` exactly — the serde
// serialisation of `crates/api/src/savings.rs::SavingsSummaryResponse`. Field
// names are the serialized (snake_case) names; do not rename them here or the
// JSON won't bind. All money is integer cents (never floats); render it with
// `formatEuros` from `$lib/api/optimise`.

/**
 * One calendar month's figures, grouped by the list's archive (shop) date — not
 * the row creation timestamp. Only the events of one status feed a bucket.
 */
export interface MonthlyBucket {
	month: string;
	gross_cents: number;
	accepted_cents: number;
	net_cents: number;
	lists: number;
}

/** One month's total-only baseline, manual spend, travel, and variance. */
export interface LegacyMonthlyBucket {
	month: string;
	baseline_cents: number;
	manual_total_cents: number;
	travel_cents: number;
	variance_cents: number;
	lists: number;
}

/**
 * One aggregate of events sharing a status, with the short copy that names its
 * baseline and included costs. `saved` counts only `verified` events; `legacy`
 * presents total-only basket variances separately.
 */
export interface SavingsAggregate {
	label: string;
	note: string;
	baseline_label: string;
	costs_note: string;
	cumulative_cents: number;
	monthly: MonthlyBucket[];
}

/** Legacy totals with no gross estimate or verified-saving fields. */
export interface LegacySavingsAggregate {
	label: string;
	note: string;
	baseline_label: string;
	costs_note: string;
	cumulative_cents: number;
	monthly: LegacyMonthlyBucket[];
}

/**
 * One savings event's outcome. `status` is the honest label of what the row
 * proves: `estimated` (like-for-like opportunity, not a saving), `unreconciled`
 * (plans price different lines, so no saving is claimed), `unchecked`
 * (comparability was not checked), or
 * `legacy_total_only` (a manual whole-basket total). `verified` is reserved and
 * never produced yet. A null `chosen_basket_cost_cents` drives the "Enter actual
 * spend" prompt in the UI.
 *
 * Issue #117 slice 2: `covered_subtotal_cents`/`covered_lines`/`total_lines`/
 * `price_date`/`covered_label` come from the run's frozen coverage snapshot.
 * `full_basket_saving_cents` is null when the plans price different lines — the
 * full-basket subtraction is suppressed and the covered subtotal shown instead.
 * All covered fields are null for an event whose run predates the snapshot.
 */
export interface ListOutcome {
	id: string;
	list_id: string;
	list_name: string;
	archived_at: string;
	status: 'estimated' | 'unreconciled' | 'unchecked' | 'legacy_total_only' | 'verified';
	status_label: string;
	baseline_label: string;
	coverage_label: string;
	costs_note: string;
	gross_saving_cents: number;
	/** The claimable full-basket saving; null when coverage is unequal. */
	full_basket_saving_cents: number | null;
	/** Recommended plan's cost over the common scope; null without a snapshot. */
	covered_subtotal_cents: number | null;
	/** `best_single − recommended` over the common scope; null without a snapshot. */
	covered_saving_cents: number | null;
	/** Lines both plans price (the covered numerator); null without a snapshot. */
	covered_lines: number | null;
	/** Active basket lines submitted (the denominator); null without a snapshot. */
	total_lines: number | null;
	/** Oldest observed offer date among the offers used; null when undated. */
	price_date: string | null;
	/** Short "Covered subtotal: N of M lines, prices as of DATE" label. */
	covered_label: string | null;
	chosen_basket_cost_cents: number | null;
	basket_variance_cents: number | null;
}

/** The full `GET /api/savings/summary` 200 response body. */
export interface SavingsSummary {
	/** The verified-only "saved" aggregate (empty until reconciliation exists). */
	saved: SavingsAggregate;
	/** The legacy total-only basket variance, shown separately. */
	legacy: LegacySavingsAggregate;
	/** Per-event outcomes, newest archive date first. */
	lists: ListOutcome[];
}

/**
 * Fetch the household's savings summary from `GET /api/savings/summary`. The
 * session cookie is sent automatically (same-origin); a GET needs no CSRF token.
 * Throws on a non-OK response so the caller can show an error state — the error
 * body is not a stable contract, so only the status is surfaced.
 */
export async function fetchSavingsSummary(): Promise<SavingsSummary> {
	const res = await fetch('/api/savings/summary');
	if (!res.ok) {
		throw new Error(`savings summary failed (${res.status})`);
	}
	return (await res.json()) as SavingsSummary;
}
