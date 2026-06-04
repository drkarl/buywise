import { describe, expect, it } from 'vitest';

import {
	sortByStoreOrder,
	UNCATEGORISED_ID,
	type CategoryGroup
} from './sortByStoreOrder';
import type {
	LocalCategory,
	LocalListEntry,
	LocalStoreCategoryOrder
} from '$lib/db/dexie';

// Three real category ids (mirror the seeded UUIDs' role, values are arbitrary
// for the pure function under test).
const PRODUCE = 'cat-produce';
const DAIRY = 'cat-dairy';
const BAKERY = 'cat-bakery';

const categories: LocalCategory[] = [
	{ id: PRODUCE, name_en: 'Produce', name_es: 'Frutas', icon: 'carrot', default_order: 1 },
	{ id: DAIRY, name_en: 'Dairy', name_es: 'Lácteos', icon: 'milk', default_order: 3 },
	{ id: BAKERY, name_en: 'Bakery', name_es: 'Panadería', icon: 'bread', default_order: 4 }
];

/** Builds a minimal active entry in a given category. */
function entry(
	id: string,
	category_id: string | undefined,
	status: LocalListEntry['status'] = 'active'
): LocalListEntry {
	return {
		id,
		list_id: 'list-1',
		resolution_status: 'resolved',
		raw_text: id,
		display_name: id,
		quantity: 1,
		status,
		category_id,
		last_updated_revision: 0,
		updated_at: '2026-01-01T00:00:00Z'
	};
}

/** Builds a store-order row. */
function order(store: string, category_id: string, display_order: number): LocalStoreCategoryOrder {
	return { id: `sco-${store}-${category_id}`, store, category_id, display_order };
}

/** Convenience: the ordered list of category ids in the result. */
const ids = (groups: CategoryGroup[]) => groups.map((g) => g.category_id);

describe('sortByStoreOrder', () => {
	it('returns an empty array for empty inputs', () => {
		expect(sortByStoreOrder([], [], 'tesco', [])).toEqual([]);
		// Items but no orders/categories still groups (falls back to category id).
		expect(sortByStoreOrder([], [order('tesco', PRODUCE, 1)], 'tesco', categories)).toEqual([]);
	});

	it('orders categories by the selected store display_order', () => {
		const items = [entry('a', DAIRY), entry('b', PRODUCE), entry('c', BAKERY)];
		// Tesco walks produce → dairy → bakery.
		const tescoOrders = [
			order('tesco', PRODUCE, 1),
			order('tesco', DAIRY, 2),
			order('tesco', BAKERY, 3)
		];

		const groups = sortByStoreOrder(items, tescoOrders, 'tesco', categories);
		expect(ids(groups)).toEqual([PRODUCE, DAIRY, BAKERY]);
	});

	it('switches ordering when a different store is selected', () => {
		const items = [entry('a', DAIRY), entry('b', PRODUCE), entry('c', BAKERY)];
		const orders = [
			// Tesco: produce, dairy, bakery
			order('tesco', PRODUCE, 1),
			order('tesco', DAIRY, 2),
			order('tesco', BAKERY, 3),
			// Aldi: bakery, dairy, produce (reversed aisle layout)
			order('aldi', BAKERY, 1),
			order('aldi', DAIRY, 2),
			order('aldi', PRODUCE, 3)
		];

		expect(ids(sortByStoreOrder(items, orders, 'tesco', categories))).toEqual([
			PRODUCE,
			DAIRY,
			BAKERY
		]);
		expect(ids(sortByStoreOrder(items, orders, 'aldi', categories))).toEqual([
			BAKERY,
			DAIRY,
			PRODUCE
		]);
	});

	it('ignores orders belonging to other stores', () => {
		const items = [entry('a', PRODUCE), entry('b', DAIRY)];
		// Only a lidl row exists; selecting tesco must NOT use lidl's order and
		// instead fall back to category default_order (produce=1 before dairy=3).
		const orders = [order('lidl', DAIRY, 1), order('lidl', PRODUCE, 99)];

		expect(ids(sortByStoreOrder(items, orders, 'tesco', categories))).toEqual([PRODUCE, DAIRY]);
	});

	it('falls back to category default_order for categories absent from the store order', () => {
		const items = [entry('a', BAKERY), entry('b', PRODUCE), entry('c', DAIRY)];
		// Store curates only DAIRY (display_order 0 → first). PRODUCE and BAKERY are
		// absent, so they fall back to default_order (produce=1 before bakery=4),
		// both AFTER the curated dairy.
		const orders = [order('tesco', DAIRY, 0)];

		expect(ids(sortByStoreOrder(items, orders, 'tesco', categories))).toEqual([
			DAIRY,
			PRODUCE,
			BAKERY
		]);
	});

	it('buckets entries with no category_id last under UNCATEGORISED_ID', () => {
		const items = [entry('u', undefined), entry('p', PRODUCE)];
		const orders = [order('tesco', PRODUCE, 1)];

		const groups = sortByStoreOrder(items, orders, 'tesco', categories);
		expect(ids(groups)).toEqual([PRODUCE, UNCATEGORISED_ID]);
		// The uncategorised group has no resolved category metadata.
		expect(groups[1].category).toBeUndefined();
	});

	it('is deterministic for categories with equal ordering keys (category_id tiebreak)', () => {
		// No store orders and no category metadata: every category falls to the max
		// key, so ordering is the stable category_id tiebreak (alphabetical).
		const items = [entry('a', 'cat-z'), entry('b', 'cat-a'), entry('c', 'cat-m')];

		const groups = sortByStoreOrder(items, [], 'tesco', []);
		expect(ids(groups)).toEqual(['cat-a', 'cat-m', 'cat-z']);
	});

	it('preserves entry order within a category', () => {
		const items = [
			entry('first', PRODUCE),
			entry('second', PRODUCE),
			entry('third', PRODUCE)
		];
		const groups = sortByStoreOrder(items, [order('tesco', PRODUCE, 1)], 'tesco', categories);

		expect(groups[0].items.map((i) => i.id)).toEqual(['first', 'second', 'third']);
	});

	it('counts only active items in remaining (bought items stay in the group)', () => {
		const items = [
			entry('a', PRODUCE, 'active'),
			entry('b', PRODUCE, 'bought'),
			entry('c', PRODUCE, 'active')
		];
		const groups = sortByStoreOrder(items, [order('tesco', PRODUCE, 1)], 'tesco', categories);

		expect(groups).toHaveLength(1);
		expect(groups[0].items).toHaveLength(3); // bought item kept in place
		expect(groups[0].remaining).toBe(2); // only the two active items count
	});

	it('attaches resolved category metadata to each group', () => {
		const groups = sortByStoreOrder(
			[entry('a', DAIRY)],
			[order('tesco', DAIRY, 1)],
			'tesco',
			categories
		);
		expect(groups[0].category?.name_en).toBe('Dairy');
	});
});
