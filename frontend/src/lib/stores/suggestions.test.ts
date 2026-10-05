import { beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import { db } from '$lib/db/dexie';
import { createSuggestionsStore, type Suggestion } from './suggestions';

const LIST = 'list-1';

const MILK: Suggestion = {
	canonical_item_id: 'ci-milk',
	display_name: 'Milk',
	typical_interval_days: 7,
	last_purchased_on: '2026-06-23',
	days_since_last_purchase: 7
};

function jsonResponse(body: unknown, ok = true): Response {
	return {
		ok,
		json: async () => body
	} as unknown as Response;
}

beforeEach(async () => {
	await db.delete();
	await db.open();
});

describe('createSuggestionsStore', () => {
	it('loads suggestions from the endpoint on refresh', async () => {
		const fetchFn = vi.fn(async () => jsonResponse({ suggestions: [MILK] }));
		const store = createSuggestionsStore(LIST, fetchFn as unknown as typeof fetch);

		await store.refresh();

		expect(fetchFn).toHaveBeenCalledWith(`/api/list/${LIST}/suggestions`);
		expect(get(store)).toEqual([MILK]);
	});

	it('keeps the last list when the request fails (offline)', async () => {
		const fetchFn = vi.fn(async () => {
			throw new Error('offline');
		});
		const store = createSuggestionsStore(LIST, fetchFn as unknown as typeof fetch);

		await store.refresh();

		expect(get(store)).toEqual([]);
	});

	it('confirm hides the suggestion and queues an add_item', async () => {
		const fetchFn = vi.fn(async () => jsonResponse({ suggestions: [MILK] }));
		const store = createSuggestionsStore(LIST, fetchFn as unknown as typeof fetch);
		await store.refresh();

		await store.confirm(MILK);

		expect(get(store)).toEqual([]);
		const muts = await db.mutation_queue.toArray();
		expect(muts).toHaveLength(1);
		expect(muts[0]).toMatchObject({ operation_type: 'add_item' });
		expect(muts[0].payload).toMatchObject({ canonical_item_id: 'ci-milk' });
	});

	it('a refresh after confirm does not resurrect the handled item', async () => {
		const fetchFn = vi.fn(async () => jsonResponse({ suggestions: [MILK] }));
		const store = createSuggestionsStore(LIST, fetchFn as unknown as typeof fetch);
		await store.refresh();

		await store.confirm(MILK);
		await store.refresh();

		expect(get(store)).toEqual([]);
	});

	it('dismiss hides the suggestion and queues a dismiss_suggestion', async () => {
		const fetchFn = vi.fn(async () => jsonResponse({ suggestions: [MILK] }));
		const store = createSuggestionsStore(LIST, fetchFn as unknown as typeof fetch);
		await store.refresh();

		await store.dismiss(MILK);

		expect(get(store)).toEqual([]);
		const muts = await db.mutation_queue.toArray();
		expect(muts).toHaveLength(1);
		expect(muts[0]).toMatchObject({
			operation_type: 'dismiss_suggestion',
			entity_type: 'canonical_item',
			entity_id: 'ci-milk'
		});
	});
});
