import { describe, expect, it } from 'vitest';

import { allocatePlan } from './allocatePlan';
import type {
	NeedsConfirmationItem,
	NoValidOfferItem,
	PlanLine,
	StorePlan,
	UnresolvedItem
} from '$lib/api/optimise';

/** Builds a priced plan line keyed to a list entry. */
function line(overrides: Partial<PlanLine> & Pick<PlanLine, 'entry_id' | 'retailer'>): PlanLine {
	return {
		item_index: 0,
		display_text: 'item',
		product_name: 'Some Product',
		canonical_item_id: 'ci-1',
		retailer_product_id: 'rp-1',
		offer_id: 'of-1',
		unit_price_cents: 100,
		line_total_cents: 100,
		loyalty_applied: false,
		freshness_confidence: 'high',
		...overrides
	};
}

/** Builds a plan over the given stores/lines. */
function plan(stores: StorePlan['stores'], lines: PlanLine[]): StorePlan {
	return {
		stores,
		lines,
		total_cents: lines.reduce((sum, l) => sum + l.line_total_cents, 0),
		missing_count: 0,
		dunnes_voucher_offer_text: null
	};
}

const noUnallocated = {
	unresolvedItems: [] as UnresolvedItem[],
	noValidOfferItems: [] as NoValidOfferItem[],
	needsConfirmationItems: [] as NeedsConfirmationItem[]
};

describe('allocatePlan', () => {
	it('splits a two-store plan into one allocation per store', () => {
		const allocation = allocatePlan({
			plan: plan(
				['tesco', 'lidl'],
				[
					line({ entry_id: 'e-bread', retailer: 'tesco' }),
					line({ entry_id: 'e-milk', retailer: 'lidl', item_index: 1 })
				]
			),
			...noUnallocated
		});

		expect(allocation.stores).toEqual(['tesco', 'lidl']);
		expect(allocation.byStore).toEqual([
			{
				retailer: 'tesco',
				lines: [expect.objectContaining({ entryId: 'e-bread', retailer: 'tesco' })]
			},
			{
				retailer: 'lidl',
				lines: [expect.objectContaining({ entryId: 'e-milk', retailer: 'lidl' })]
			}
		]);
		expect(allocation.unallocated).toEqual([]);
		// Both lists together account for the whole plan, once each.
		expect(allocation.entryIds).toEqual(['e-bread', 'e-milk']);
	});

	it('keeps unresolved, unpriceable and unconfirmed items in an explicit unallocated section', () => {
		const allocation = allocatePlan({
			plan: plan(['tesco', 'lidl'], [line({ entry_id: 'e-milk', retailer: 'lidl' })]),
			unresolvedItems: [{ item_index: 2, entry_id: 'e-cream', display_text: 'cream' }],
			noValidOfferItems: [
				{ item_index: 3, entry_id: 'e-truffle', display_text: 'truffle oil', reason: 'no_matches' }
			],
			needsConfirmationItems: [
				{
					item_index: 4,
					entry_id: 'e-cheese',
					display_text: 'cheese',
					canonical_item_id: 'ci-cheese',
					candidate_count: 0,
					candidates: []
				}
			]
		});

		// Ordered by the list position the optimiser saw (`item_index`).
		expect(allocation.unallocated).toEqual([
			{ entryId: 'e-cream', reason: 'unresolved' },
			{ entryId: 'e-truffle', reason: 'no_offer' },
			{ entryId: 'e-cheese', reason: 'needs_confirmation' }
		]);
		// Nothing unallocated leaks into a store bucket.
		expect(allocation.byStore.flatMap((s) => s.lines.map((l) => l.entryId))).toEqual(['e-milk']);
		expect(allocation.entryIds).toEqual(['e-milk', 'e-cream', 'e-truffle', 'e-cheese']);
	});

	it('counts a multi-quantity entry exactly once', () => {
		// A quantity-3 entry produces ONE plan line whose total is unit × 3.
		const allocation = allocatePlan({
			plan: plan(['aldi'], [line({ entry_id: 'e-eggs', retailer: 'aldi', unit_price_cents: 250, line_total_cents: 750 })]),
			...noUnallocated
		});

		expect(allocation.byStore[0].lines).toHaveLength(1);
		expect(allocation.byStore[0].lines[0].lineTotalCents).toBe(750);
		// The entry is accounted for once: no duplicate row, so a per-store view
		// renders one "×3" line and never double-counts the quantity.
		expect(allocation.entryIds).toEqual(['e-eggs']);
	});

	it('accounts for every entry exactly once across stores and the unallocated section', () => {
		const allocation = allocatePlan({
			plan: plan(
				['tesco', 'lidl'],
				[line({ entry_id: 'e-bread', retailer: 'tesco' }), line({ entry_id: 'e-milk', retailer: 'lidl' })]
			),
			unresolvedItems: [{ item_index: 9, entry_id: 'e-cream', display_text: 'cream' }],
			noValidOfferItems: [],
			needsConfirmationItems: []
		});

		const all = allocation.entryIds;
		expect(new Set(all).size).toBe(all.length);
		expect(new Set(all)).toEqual(new Set(['e-bread', 'e-milk', 'e-cream']));
	});
});
