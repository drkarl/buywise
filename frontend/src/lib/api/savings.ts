// Client types + helper for the savings dashboard (P5-T4).
//
// These mirror the wire shape of `GET /api/savings/summary` exactly — the serde
// serialisation of `crates/api/src/savings.rs::SavingsSummaryResponse`. Field
// names are the serialized (snake_case) names; do not rename them here or the
// JSON won't bind. All money is integer cents (never floats); render it with
// `formatEuros` from `$lib/api/optimise`.

/**
 * One calendar month's aggregate savings. `month` is `"YYYY-MM"` (UTC).
 * `accepted_cents`/`net_cents` treat un-reconciled (null) rows as 0 on the
 * server, so a month with no actual-spend entered yet reads as €0 net rather
 * than a skewed figure. `lists` is the count of archived lists in the month.
 */
export interface MonthlySummary {
	month: string;
	gross_cents: number;
	accepted_cents: number;
	net_cents: number;
	lists: number;
}

/**
 * One archived list's outcome, for the per-list table. `chosen_basket_cost_cents`
 * and `net_saving_cents` are null until the household posts their actual spend;
 * a null `chosen_basket_cost_cents` is what drives the "Enter actual spend"
 * prompt in the UI.
 */
export interface ListOutcome {
	id: string;
	list_id: string;
	list_name: string;
	created_at: string;
	gross_saving_cents: number;
	chosen_basket_cost_cents: number | null;
	net_saving_cents: number | null;
}

/** The full `GET /api/savings/summary` 200 response body. */
export interface SavingsSummary {
	/** Per-month aggregates, ordered chronologically (oldest first). */
	monthly: MonthlySummary[];
	/** Sum of `net_saving_cents` across all non-null rows. */
	cumulative_net_cents: number;
	/** Per-archived-list outcomes, newest first. */
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
