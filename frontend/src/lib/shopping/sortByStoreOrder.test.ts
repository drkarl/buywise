import { describe, expect, it } from 'vitest';

import {
	sortByStoreOrder,
	UNCATEGORISED_ID,
	type CategoryGroup
} from './sortByStoreOrder';
import type {
	LocalCategory,
	LocalListEntry,
	LocalStoreCategoryOrder,
	LocalSubcategory
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
	status: LocalListEntry['status'] = 'active',
	subcategory_id?: string
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
		subcategory_id,
		last_updated_revision: 0,
		updated_at: '2026-01-01T00:00:00Z'
	};
}

/** Builds a store-order row. */
function order(store: string, category_id: string, display_order: number): LocalStoreCategoryOrder {
	return { id: `sco-${store}-${category_id}`, store, category_id, display_order };
}

/** Convenience: the ordered list of section ids in the result. */
const ids = (groups: CategoryGroup[]) => groups.map((g) => g.id);

describe('sortByStoreOrder', () => {
	it('returns an empty array for empty inputs', () => {
		expect(sortByStoreOrder([], [], 'tesco', [])).toEqual([]);
		// No items still yields no groups even when orders/categories are present.
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

	it('orders curated, then known-but-absent, then uncategorised (three-tier precedence)', () => {
		// One curated category (DAIRY), one known category absent from the store
		// order (PRODUCE, falls back to default_order), and one uncategorised entry.
		// Expected: curated first, then the default-ordered known category, then the
		// uncategorised bucket last — regardless of input order.
		const items = [entry('u', undefined), entry('p', PRODUCE), entry('d', DAIRY)];
		const orders = [order('tesco', DAIRY, 5)];

		const groups = sortByStoreOrder(items, orders, 'tesco', categories);
		expect(ids(groups)).toEqual([DAIRY, PRODUCE, UNCATEGORISED_ID]);
	});

	it('keeps ordering deterministic even when stored orders are huge (no overflow)', () => {
		// A hostile/large curated display_order must still precede default-ordered
		// categories, and large default_orders must not collapse into one another.
		const bigCats: LocalCategory[] = [
			{ ...categories[0], id: PRODUCE, default_order: Number.MAX_SAFE_INTEGER - 1 },
			{ ...categories[1], id: DAIRY, default_order: Number.MAX_SAFE_INTEGER }
		];
		const items = [entry('p', PRODUCE), entry('d', DAIRY), entry('b', BAKERY)];
		// BAKERY is curated with a large order; it must still come before the two
		// default-ordered categories. PRODUCE (smaller default) precedes DAIRY.
		const orders = [order('tesco', BAKERY, Number.MAX_SAFE_INTEGER)];

		const groups = sortByStoreOrder(items, orders, 'tesco', bigCats);
		expect(ids(groups)).toEqual([BAKERY, PRODUCE, DAIRY]);
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

describe('sortByStoreOrder sub-categories (issue #101)', () => {
	const PANTRY = 'cat-pantry';
	const PASTA = 'sub-pasta';
	const TINS = 'sub-tins';

	const pantryCategories: LocalCategory[] = [
		{ id: PRODUCE, name_en: 'Produce', name_es: 'Frutas', icon: 'carrot', default_order: 1 },
		{ id: PANTRY, name_en: 'Pantry', name_es: 'Despensa', icon: 'package', default_order: 5 }
	];

	const subcategories: LocalSubcategory[] = [
		{
			id: TINS,
			category_id: PANTRY,
			name_en: 'Tins & Jars',
			name_es: 'Conservas y Tarros',
			default_order: 2
		},
		{
			id: PASTA,
			category_id: PANTRY,
			name_en: 'Pasta & Rice',
			name_es: 'Pasta y Arroz',
			default_order: 1
		}
	];

	it('splits a broad category into sub-sections in default_order', () => {
		const items = [entry('beans', PANTRY, 'active', TINS), entry('pasta', PANTRY, 'active', PASTA)];

		const groups = sortByStoreOrder(items, [], 'tesco', pantryCategories, subcategories);

		// Pasta (order 1) before Tins (order 2), even though beans arrived first.
		expect(ids(groups)).toEqual([PASTA, TINS]);
		expect(groups[0].subcategory?.name_en).toBe('Pasta & Rice');
		expect(groups[0].subcategory?.name_es).toBe('Pasta y Arroz');
		// Each sub-section still carries its parent category for ordering/context.
		expect(groups[0].category_id).toBe(PANTRY);
		expect(groups[0].category?.name_en).toBe('Pantry');
	});

	it('keeps the sub-sections inside the parent category store slot', () => {
		const items = [
			entry('apple', PRODUCE),
			entry('pasta', PANTRY, 'active', PASTA),
			entry('beans', PANTRY, 'active', TINS)
		];
		// Store curates Pantry (1) BEFORE Produce (2): both Pantry sub-sections
		// must stay contiguous under that slot.
		const orders = [order('tesco', PANTRY, 1), order('tesco', PRODUCE, 2)];

		const groups = sortByStoreOrder(items, orders, 'tesco', pantryCategories, subcategories);
		expect(ids(groups)).toEqual([PASTA, TINS, PRODUCE]);
	});

	it('falls back to the parent category for an item with no sub-category', () => {
		// One mapped item and one unmapped item in the same parent category.
		const items = [entry('flour', PANTRY), entry('pasta', PANTRY, 'active', PASTA)];

		const groups = sortByStoreOrder(items, [], 'tesco', pantryCategories, subcategories);
		// Split sub-category first; the unmapped item keeps the parent category id.
		expect(ids(groups)).toEqual([PASTA, PANTRY]);
		expect(groups[1].subcategory).toBeUndefined();
		expect(groups[1].category?.name_en).toBe('Pantry');
		expect(groups[1].items.map((i) => i.id)).toEqual(['flour']);
	});

	it('ignores a stale subcategory_id absent from the catalogue', () => {
		// The entry references a sub-category the client has not cached (e.g. a
		// newer split than the cached catalogue): fall back to the parent category
		// rather than dropping the item.
		const items = [entry('mystery', PANTRY, 'active', 'sub-missing')];

		const groups = sortByStoreOrder(items, [], 'tesco', pantryCategories, subcategories);
		expect(ids(groups)).toEqual([PANTRY]);
	});

	it('keeps entries with no category at all in the uncategorised bucket last', () => {
		const items = [
			entry('u', undefined),
			entry('pasta', PANTRY, 'active', PASTA)
		];

		const groups = sortByStoreOrder(items, [], 'tesco', pantryCategories, subcategories);
		expect(ids(groups)).toEqual([PASTA, UNCATEGORISED_ID]);
	});
});
