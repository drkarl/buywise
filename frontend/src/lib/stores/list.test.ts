import { beforeEach, describe, expect, it } from 'vitest';
import { get } from 'svelte/store';
import { db } from '$lib/db/dexie';
import {
	addItem,
	markBought,
	markSkipped,
	deleteItem,
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

		// `deleted` is terminal: a racing double-tap / swipe-delete on an already
		// soft-deleted entry is an idempotent no-op — no redundant delete_item.
		await deleteItem(LIST, id);
		expect(await db.mutation_queue.count()).toBe(after);
	});
});

describe('delete-wins guard — no resurrection of a deleted entry', () => {
	it('markBought on a deleted entry does not resurrect it and queues NO mutation', async () => {
		const id = await addItem(LIST, 'Mustard');
		await deleteItem(LIST, id);
		const after = await db.mutation_queue.count();

		await markBought(LIST, id);

		// Mirrors the backend's "delete wins": status stays 'deleted'.
		expect((await db.list_entries.get(id))?.status).toBe('deleted');
		expect(await db.mutation_queue.count()).toBe(after);
		const boughtMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'mark_bought'
		);
		expect(boughtMut).toBeUndefined();
	});

	it('markSkipped on a deleted entry does not resurrect it and queues NO mutation', async () => {
		const id = await addItem(LIST, 'Relish');
		await deleteItem(LIST, id);
		const after = await db.mutation_queue.count();

		await markSkipped(LIST, id);

		expect((await db.list_entries.get(id))?.status).toBe('deleted');
		expect(await db.mutation_queue.count()).toBe(after);
		const skipMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'mark_skipped'
		);
		expect(skipMut).toBeUndefined();
	});
});

describe('list_id / entryId mismatch guard', () => {
	it('markBought with a listId that does not match the entry mutates nothing and queues no mutation', async () => {
		const id = await addItem(LIST, 'Limes');
		const before = await db.mutation_queue.count();

		await markBought('other-list', id);

		// Entry untouched: still active, still on the original list.
		const entry = await db.list_entries.get(id);
		expect(entry?.status).toBe('active');
		expect(entry?.list_id).toBe(LIST);

		// No mark_bought mutation enqueued.
		expect(await db.mutation_queue.count()).toBe(before);
		const boughtMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'mark_bought'
		);
		expect(boughtMut).toBeUndefined();
	});

	it('deleteItem with a listId that does not match the entry mutates nothing and queues no mutation', async () => {
		const id = await addItem(LIST, 'Lemons');
		const before = await db.mutation_queue.count();

		await deleteItem('other-list', id);

		const entry = await db.list_entries.get(id);
		expect(entry?.status).toBe('active');
		expect(entry?.list_id).toBe(LIST);

		expect(await db.mutation_queue.count()).toBe(before);
		const delMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'delete_item'
		);
		expect(delMut).toBeUndefined();
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
