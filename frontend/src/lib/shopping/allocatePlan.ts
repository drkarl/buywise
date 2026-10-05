// Pure allocation of a chosen optimiser plan to per-store trips (issue #104).
//
// The optimiser returns priced plan lines, excluded items, and identity metadata
// for every input item. This module turns that
// response — for ONE chosen plan — into a derived view over the household's list
// entries: every priced line is assigned to the store that fulfils it, and every
// item the plan does not allocate lands in an explicit `unallocated` list.
//
// No Dexie, Svelte, or DOM here so it is trivially unit-testable. The per-store
// and unallocated lists are VIEWS: they hold entry IDs (and the chosen product
// for an allocated line), never copies of the list entries themselves, so a
// check-off on any view updates the one shared entry through the normal offline
// queue.
//
// Join key: `PlanLine.entry_id` / `UnresolvedItem.entry_id` / … — the list entry
// id the optimiser echoed back (see `crates/core/src/optimiser.rs::BasketItem`).
// A line with no `entry_id` (a caller with no persisted line) cannot be attached
// to a household entry and is skipped; the API always sets it for a stored list.

import type {
	BasketItemIdentity,
	FreshnessConfidence,
	NeedsConfirmationItem,
	NoValidOfferItem,
	StorePlan,
	UnresolvedItem
} from '$lib/api/optimise';
import type { RetailerId } from '$lib/stores/shopping';

/** Why an item the plan did not allocate was left out. */
export type UnallocatedReason =
	| 'unresolved'
	| 'no_offer'
	| 'needs_confirmation'
	| 'not_in_plan';

/** One priced plan line, assigned to its store and keyed by list entry. */
export interface AllocatedLine {
	/** The household list entry this line fulfils. */
	entryId: string;
	retailer: RetailerId;
	retailerProductId: string;
	/** The chosen product's display name (shown on the trip list). */
	productName: string;
	unitPriceCents: number;
	lineTotalCents: number;
	loyaltyApplied: boolean;
	freshnessConfidence: FreshnessConfidence;
}

/** One list entry the plan could not allocate, with the reason. */
export interface UnallocatedLine {
	entryId: string;
	reason: UnallocatedReason;
}

/** The allocation for one store: its priced lines, in plan order. */
export interface StoreAllocation {
	retailer: RetailerId;
	lines: AllocatedLine[];
}

/** The chosen plan expressed as a view over the household's list entries. */
export interface PlanAllocation {
	/** The plan's stores, in the order the plan reports them. */
	stores: RetailerId[];
	/** One entry per store, in `stores` order (empty lines included). */
	byStore: StoreAllocation[];
	/** Items the plan does not allocate, ordered by list position. */
	unallocated: UnallocatedLine[];
	/** Every accounted-for entry id (allocated first, then unallocated). */
	entryIds: string[];
}

/**
 * Allocate `plan` to its stores over the household's list entries.
 *
 * Every submitted `entry_id` lands in exactly one of `byStore[*].lines` or
 * `unallocated`, so the two together account for the whole basket and no entry
 * is counted twice. `plan.stores` fixes the store order; `unallocated` preserves
 * the optimiser's `item_index` order (i.e. the list order the optimiser saw).
 */
export function allocatePlan(args: {
	plan: StorePlan;
	unresolvedItems: UnresolvedItem[];
	noValidOfferItems: NoValidOfferItem[];
	needsConfirmationItems: NeedsConfirmationItem[];
	basketItems: BasketItemIdentity[];
}): PlanAllocation {
	const { plan, unresolvedItems, noValidOfferItems, needsConfirmationItems, basketItems } = args;

	const stores: RetailerId[] = [...plan.stores];
	const linesByStore: Record<string, AllocatedLine[]> = {};
	for (const store of stores) linesByStore[store] = [];

	const seen = new Set<string>();
	for (const line of plan.lines) {
		// A line with no entry id cannot be attached to a household entry; the API
		// always sets it, so this only guards a hand-built/legacy response.
		if (line.entry_id === null || seen.has(line.entry_id)) continue;
		// A store the plan did not declare is impossible from the core; skip
		// defensively rather than render an orphan list.
		const bucket = linesByStore[line.retailer];
		if (!bucket) continue;
		seen.add(line.entry_id);
		bucket.push({
			entryId: line.entry_id,
			retailer: line.retailer,
			retailerProductId: line.retailer_product_id,
			productName: line.product_name,
			unitPriceCents: line.unit_price_cents,
			lineTotalCents: line.line_total_cents,
			loyaltyApplied: line.loyalty_applied,
			freshnessConfidence: line.freshness_confidence
		});
	}

	// Unallocated items, merged and ordered by the list position the optimiser
	// saw (`item_index`), so the explicit section reads in list order.
	const unallocatedByIndex: Array<{ item_index: number; entryId: string; reason: UnallocatedReason }> =
		[];
	const push = (item: { item_index: number; entry_id: string | null }, reason: UnallocatedReason) => {
		if (item.entry_id === null || seen.has(item.entry_id)) return;
		seen.add(item.entry_id);
		unallocatedByIndex.push({ item_index: item.item_index, entryId: item.entry_id, reason });
	};
	for (const item of unresolvedItems) push(item, 'unresolved');
	for (const item of noValidOfferItems) push(item, 'no_offer');
	for (const item of needsConfirmationItems) push(item, 'needs_confirmation');
	for (const item of basketItems) push(item, 'not_in_plan');
	unallocatedByIndex.sort((a, b) => a.item_index - b.item_index);

	const unallocated: UnallocatedLine[] = unallocatedByIndex.map(({ entryId, reason }) => ({
		entryId,
		reason
	}));

	const byStore: StoreAllocation[] = stores.map((retailer) => ({
		retailer,
		lines: linesByStore[retailer] ?? []
	}));

	return {
		stores,
		byStore,
		unallocated,
		entryIds: [
			...byStore.flatMap((s) => s.lines.map((l) => l.entryId)),
			...unallocated.map((u) => u.entryId)
		]
	};
}
