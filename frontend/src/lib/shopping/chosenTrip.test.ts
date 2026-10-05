import { beforeEach, describe, expect, it } from 'vitest';

import {
	clearChosenTrip,
	isTripStale,
	loadChosenTrip,
	saveChosenTrip,
	type ChosenTrip
} from './chosenTrip';

function trip(entryIds: string[]): ChosenTrip {
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
		}
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
});
