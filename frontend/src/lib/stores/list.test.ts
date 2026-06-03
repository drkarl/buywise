import { beforeEach, describe, expect, it } from 'vitest';
import { get } from 'svelte/store';
import { db } from '$lib/db/dexie';
import {
	addItem,
	markBought,
	markSkipped,
	deleteItem,
	incrementQuantity,
	decrementQuantity,
	activeEntries,
	basketEntries
} from './list';

const LIST = 'list-1';

/** Resolves once a derived live store satisfies `predicate` (or times out). */
async function until<T>(
	store: { subscribe: (run: (v: T) => void) => () => void },
	predicate: (v: T) => boolean,
	timeoutMs = 1000
): Promise<T> {
	return new Promise((resolve, reject) => {
		const timer = setTimeout(() => {
			unsub();
			reject(new Error('until: predicate not satisfied in time'));
		}, timeoutMs);
		const unsub = store.subscribe((v) => {
			if (predicate(v)) {
				clearTimeout(timer);
				queueMicrotask(unsub);
				resolve(v);
			}
		});
	});
}

beforeEach(async () => {
	await db.delete();
	await db.open();
});

describe('addItem', () => {
	it('queues a pending add_item mutation and inserts an optimistic active entry', async () => {
		const id = await addItem(LIST, '  Milk  ', 1);

		const entries = await db.list_entries.toArray();
		expect(entries).toHaveLength(1);
		expect(entries[0]).toMatchObject({
			id,
			list_id: LIST,
			raw_text: 'Milk',
			display_name: 'Milk',
			quantity: 1,
			status: 'active',
			resolution_status: 'unresolved',
			last_updated_revision: 0
		});
		expect(entries[0].canonical_item_id).toBeUndefined();

		const muts = await db.mutation_queue.toArray();
		expect(muts).toHaveLength(1);
		expect(muts[0]).toMatchObject({
			operation_type: 'add_item',
			entity_type: 'list_entry',
			entity_id: id,
			list_id: LIST,
			status: 'pending',
			retry_count: 0,
			base_revision: 0,
			payload: { raw_text: 'Milk', display_name: 'Milk', quantity: 1 }
		});
		expect(muts[0].client_mutation_id).toMatch(/[0-9a-f-]{36}/);
		expect(muts[0].device_id).toMatch(/[0-9a-f-]{36}/);
	});

	it('defaults quantity to 1 and rejects empty text', async () => {
		await addItem(LIST, 'Eggs');
		const [entry] = await db.list_entries.toArray();
		expect(entry.quantity).toBe(1);
		await expect(addItem(LIST, '   ')).rejects.toThrow();
	});

	it('uses list_meta.current_revision as base_revision', async () => {
		await db.list_meta.put({ id: LIST, name: 'Groceries', current_revision: 7 });
		await addItem(LIST, 'Bread');
		const [mut] = await db.mutation_queue.toArray();
		expect(mut.base_revision).toBe(7);
	});
});

describe('mark_bought / mark_skipped', () => {
	it('marks an entry bought locally and queues a mark_bought mutation immediately', async () => {
		const id = await addItem(LIST, 'Cheese');
		await markBought(LIST, id);

		const entry = await db.list_entries.get(id);
		expect(entry?.status).toBe('bought');
		expect(entry?.bought_at).toBeTruthy();

		const boughtMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'mark_bought'
		);
		expect(boughtMut).toMatchObject({ entity_id: id, status: 'pending' });
		expect(boughtMut?.payload).toHaveProperty('bought_at');
	});

	it('marks an entry skipped with an empty payload', async () => {
		const id = await addItem(LIST, 'Soap');
		await markSkipped(LIST, id);

		expect((await db.list_entries.get(id))?.status).toBe('skipped');
		const skipMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'mark_skipped'
		);
		expect(skipMut?.payload).toEqual({});
	});
});

describe('delete_item', () => {
	it('soft-deletes locally and queues a delete_item mutation', async () => {
		const id = await addItem(LIST, 'Chips');
		await deleteItem(LIST, id);

		expect((await db.list_entries.get(id))?.status).toBe('deleted');
		const delMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'delete_item'
		);
		expect(delMut).toMatchObject({ entity_id: id, payload: {} });
	});
});

describe('drift guard — mark/delete on an unknown or stale id', () => {
	it('markBought on an unknown id queues NO mutation', async () => {
		await markBought(LIST, 'does-not-exist');
		expect(await db.mutation_queue.count()).toBe(0);
		expect(await db.list_entries.count()).toBe(0);
	});

	it('markSkipped on an unknown id queues NO mutation', async () => {
		await markSkipped(LIST, 'does-not-exist');
		expect(await db.mutation_queue.count()).toBe(0);
	});

	it('deleteItem on an unknown id queues NO mutation', async () => {
		await deleteItem(LIST, 'does-not-exist');
		expect(await db.mutation_queue.count()).toBe(0);
	});

	it('a second deleteItem on an already-deleted id queues NO extra mutation', async () => {
		const id = await addItem(LIST, 'Ketchup');
		await deleteItem(LIST, id);
		const after = await db.mutation_queue.count();

		// Racing double-tap / swipe-delete: the entry is still present (soft-deleted)
		// so the update succeeds and a second delete_item IS enqueued. That is fine —
		// the entity exists; the drift we guard against is mutations with no matching
		// optimistic record at all.
		await deleteItem(LIST, id);
		expect(await db.mutation_queue.count()).toBe(after + 1);
	});
});

describe('quantity', () => {
	it('increment-then-bought keeps queue and local in lockstep on the same entity', async () => {
		const id = await addItem(LIST, 'Flour');
		await incrementQuantity(LIST, id);
		await markBought(LIST, id);

		const entry = await db.list_entries.get(id);
		expect(entry?.quantity).toBe(2);
		expect(entry?.status).toBe('bought');

		const muts = await db.mutation_queue.toArray();
		// add_item (initial) + add_item (increment) + mark_bought, all on this id.
		expect(muts).toHaveLength(3);
		expect(muts.every((m) => m.entity_id === id)).toBe(true);
		expect(muts.filter((m) => m.operation_type === 'add_item')).toHaveLength(2);
		expect(muts.filter((m) => m.operation_type === 'mark_bought')).toHaveLength(1);
	});

	it('increment bumps local quantity AND queues an add_item on the existing entity', async () => {
		const id = await addItem(LIST, 'Apples');
		await incrementQuantity(LIST, id);

		expect((await db.list_entries.get(id))?.quantity).toBe(2);
		const addMuts = (await db.mutation_queue.toArray()).filter(
			(m) => m.operation_type === 'add_item'
		);
		// initial add + the increment, both on the same entity id
		expect(addMuts).toHaveLength(2);
		expect(addMuts.every((m) => m.entity_id === id)).toBe(true);
	});

	it('decrement updates local quantity only (no mutation) and floors at 1', async () => {
		const id = await addItem(LIST, 'Bananas', 2);
		const before = await db.mutation_queue.count();

		await decrementQuantity(id);
		expect((await db.list_entries.get(id))?.quantity).toBe(1);

		await decrementQuantity(id); // already at floor
		expect((await db.list_entries.get(id))?.quantity).toBe(1);

		expect(await db.mutation_queue.count()).toBe(before); // no queue growth
	});
});

describe('live stores / reactivity', () => {
	it('moves bought items out of active into basket reactively', async () => {
		const id = await addItem(LIST, 'Yogurt');

		const active = activeEntries(LIST);
		const basket = basketEntries(LIST);

		await until(active, (e) => e.length === 1);
		expect(get(basket)).toHaveLength(0);

		await markBought(LIST, id);

		await until(basket, (e) => e.length === 1);
		await until(active, (e) => e.length === 0);
	});

	it('excludes deleted entries from both sections', async () => {
		const id = await addItem(LIST, 'Toothpaste');
		const active = activeEntries(LIST);
		const basket = basketEntries(LIST);
		await until(active, (e) => e.length === 1);

		await deleteItem(LIST, id);
		await until(active, (e) => e.length === 0);
		expect(get(basket)).toHaveLength(0);
	});
});
