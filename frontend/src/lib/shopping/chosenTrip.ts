// Persistence for the household's chosen trip plan (issue #104).
//
// Choosing a plan on the optimise page stores its allocation against the list id
// so the per-store trip view and the store-filtered shopping mode can render it
// in-store, offline, without re-running the optimiser. This is a lightweight
// client-side cache, not server state: the allocation is a VIEW over the shared
// list entries, and every check-off still flows through the normal offline queue,
// so two devices that chose differently still converge on the same entries.
//
// localStorage (matching `stores/shopping.ts`'s selected-store persistence) is
// deliberately used over a Dexie table: the chosen plan is a small, per-device
// snapshot with no query needs, and localStorage needs no schema migration.

import type { PlanAllocation } from '$lib/shopping/allocatePlan';

/** The plan kind the household picked (mirrors `Recommendation.kind`). */
export type ChosenPlanKind = 'single_store' | 'two_store';

/** One chosen trip plan, as persisted for a list. */
export interface ChosenTrip {
	listId: string;
	/** The optimiser run this allocation came from (for provenance/debugging). */
	runId: string;
	planKind: ChosenPlanKind;
	/** Human label of the picked plan, e.g. "Recommended split". */
	title: string;
	/** The allocation (per-store lines + the explicit unallocated section). */
	allocation: PlanAllocation;
	/** ISO timestamp the plan was chosen. */
	savedAt: string;
}

const KEY_PREFIX = 'buywise:trip:';

/**
 * Reads the chosen trip for `listId`, or `null` when none is stored (or the
 * stored value is unreadable — a corrupted entry degrades to "no trip" rather
 * than throwing in the UI). localStorage being unavailable (private mode / SSR)
 * also degrades to `null`.
 */
export function loadChosenTrip(listId: string): ChosenTrip | null {
	if (!listId) return null;
	try {
		const raw = globalThis.localStorage?.getItem(`${KEY_PREFIX}${listId}`);
		if (!raw) return null;
		const parsed = JSON.parse(raw) as ChosenTrip;
		if (!parsed || parsed.listId !== listId || !parsed.allocation) return null;
		return parsed;
	} catch {
		return null;
	}
}

/** Persists the chosen trip for its list. A no-op if localStorage is unavailable. */
export function saveChosenTrip(trip: ChosenTrip): void {
	try {
		globalThis.localStorage?.setItem(`${KEY_PREFIX}${trip.listId}`, JSON.stringify(trip));
	} catch {
		// Non-fatal: the trip simply won't persist across reloads.
	}
}

/** Removes any chosen trip for `listId`. */
export function clearChosenTrip(listId: string): void {
	try {
		globalThis.localStorage?.removeItem(`${KEY_PREFIX}${listId}`);
	} catch {
		// Non-fatal.
	}
}

/**
 * Whether the chosen trip no longer covers the list: an entry still to buy (an
 * `active` one) is not in the plan's allocation. Adding a list item after
 * choosing makes the plan stale, so the view offers the review path back to the
 * optimiser.
 *
 * A check-off is NOT stale: the entry keeps its id and simply moves out of
 * `active` (it is done, so the plan no longer needs to cover it). A deletion is
 * not stale either — nothing is left unbought.
 */
export function isTripStale(
	trip: ChosenTrip,
	entries: ReadonlyArray<{ id: string; status: string }>
): boolean {
	const planned = new Set(trip.allocation.entryIds);
	return entries.some((e) => e.status === 'active' && !planned.has(e.id));
}
