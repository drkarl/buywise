import { beforeEach, describe, expect, it } from 'vitest';

import {
	clearChosenTrip,
	isTripStale,
	loadChosenTrip,
	saveChosenTrip,
	type ChosenTrip
} from './chosenTrip';

function trip(
	entryIds: string[],
	quantities?: Record<string, number>,
	canonicalItemIds?: Record<string, string | null>
): ChosenTrip {
	return {
		listId: 'list-1',
		runId: 'run-1',
		planKind: 'two_store',
		title: 'Recommended split',
		savedAt: '2026-01-01T00:00:00Z',
		allocation: {
			stores: ['tesco', 'aldi'],
			byStore: [
				{ retailer: 'tesco', lines: [] },
				{ retailer: 'aldi', lines: [] }
			],
			unallocated: entryIds.map((entryId) => ({ entryId, reason: 'unresolved' as const })),
			entryIds
		},
		quantities,
		canonicalItemIds
	};
}

beforeEach(() => localStorage.clear());

describe('chosenTrip', () => {
	it('round-trips a saved trip and clears it', () => {
		saveChosenTrip(trip(['e-1', 'e-2']));
		expect(loadChosenTrip('list-1')?.allocation.entryIds).toEqual(['e-1', 'e-2']);
		clearChosenTrip('list-1');
		expect(loadChosenTrip('list-1')).toBeNull();
	});

	it('treats a status change (check-off) as NOT stale', () => {
		// Same ids, one now bought: nothing is left uncovered.
		expect(
			isTripStale(trip(['e-1', 'e-2']), [
				{ id: 'e-1', status: 'bought' },
				{ id: 'e-2', status: 'active' }
			])
		).toBe(false);
	});

	it('is stale only when an active entry is not covered by the plan', () => {
		// A newly added item the plan never saw → stale.
		expect(
			isTripStale(trip(['e-1', 'e-2']), [
				{ id: 'e-1', status: 'active' },
				{ id: 'e-2', status: 'active' },
				{ id: 'e-3', status: 'active' }
			])
		).toBe(true);
		// A planned entry that is merely deleted leaves nothing unbought.
		expect(
			isTripStale(trip(['e-1', 'e-2']), [
				{ id: 'e-1', status: 'active' },
				{ id: 'e-2', status: 'deleted' }
			])
		).toBe(false);
	});

	it('treats an edited quantity on a planned entry as stale', () => {
		// The plan was chosen when Milk (e-2) had quantity 2.
		expect(
			isTripStale(trip(['e-1', 'e-2'], { 'e-1': 1, 'e-2': 2 }), [
				{ id: 'e-1', status: 'active', quantity: 1 },
				{ id: 'e-2', status: 'active', quantity: 3 }
			])
		).toBe(true);
	});

	it('is not stale when a planned entry keeps its quantity', () => {
		expect(
			isTripStale(trip(['e-1', 'e-2'], { 'e-1': 1, 'e-2': 2 }), [
				{ id: 'e-1', status: 'active', quantity: 1 },
				{ id: 'e-2', status: 'active', quantity: 2 }
			])
		).toBe(false);
	});

	it('ignores quantity on a cache written before snapshots existed', () => {
		// An old cache has no `quantities` map; it must still load and must not
		// report stale just because the entries now carry quantities.
		expect(
			isTripStale(trip(['e-1', 'e-2']), [
				{ id: 'e-1', status: 'active', quantity: 5, canonical_item_id: 'ci-1' },
				{ id: 'e-2', status: 'active', quantity: 9, canonical_item_id: 'ci-2' }
			])
		).toBe(false);
	});

	it('treats resolving a planned ambiguous entry as stale', () => {
		expect(
			isTripStale(trip(['e-1'], undefined, { 'e-1': null }), [
				{ id: 'e-1', status: 'active', canonical_item_id: 'ci-milk' }
			])
		).toBe(true);
	});
});
