import { beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import { db } from '$lib/db/dexie';
import {
	addItem,
	markBought,
	markSkipped,
	deleteItem,
	setQuantity,
	resolveAlias,
	activeEntries,
	basketEntries,
	MAX_QUANTITY
} from './list';
import type { LocalListEntry } from '$lib/db/dexie';

const LIST = 'list-1';

/** Seed an ambiguous active entry directly (as the sync apply path would). */
async function seedAmbiguous(
	id: string,
	rawText: string,
	candidates: string[]
): Promise<void> {
	const entry: LocalListEntry = {
		id,
		list_id: LIST,
		resolution_status: 'ambiguous',
		resolution_candidates_json: JSON.stringify(candidates),
		raw_text: rawText,
		display_name: rawText,
		quantity: 1,
		status: 'active',
		last_updated_revision: 0,
		updated_at: '2026-06-05T00:00:00.000Z'
	};
	await db.list_entries.add(entry);
}

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

describe('resolveAlias', () => {
	it('binds the chosen item, resolves the entry, clears candidates, and queues a resolve_alias mutation', async () => {
		await seedAmbiguous('e1', 'cream', ['ci-single', 'ci-double']);

		await resolveAlias(LIST, 'e1', 'ci-double');

		const entry = await db.list_entries.get('e1');
		expect(entry?.resolution_status).toBe('resolved');
		expect(entry?.canonical_item_id).toBe('ci-double');
		expect(entry?.resolution_candidates_json).toBeUndefined();

		const muts = await db.mutation_queue.toArray();
		expect(muts).toHaveLength(1);
		expect(muts[0]).toMatchObject({
			operation_type: 'resolve_alias',
			entity_id: 'e1',
			status: 'pending',
			payload: { chosen_canonical_item_id: 'ci-double', raw_text: 'cream' }
		});
	});

	it('is a no-op when the entry is not ambiguous (does not spam the server)', async () => {
		// A resolved entry must not re-enqueue a confirmation.
		await db.list_entries.add({
			id: 'e2',
			list_id: LIST,
			resolution_status: 'resolved',
			canonical_item_id: 'ci-x',
			raw_text: 'milk',
			display_name: 'Milk',
			quantity: 1,
			status: 'active',
			last_updated_revision: 0,
			updated_at: '2026-06-05T00:00:00.000Z'
		});

		await resolveAlias(LIST, 'e2', 'ci-y');

		const entry = await db.list_entries.get('e2');
		expect(entry?.canonical_item_id).toBe('ci-x'); // untouched
		expect(await db.mutation_queue.count()).toBe(0);
	});

	it('is a no-op for an entry on a different list', async () => {
		await db.list_entries.add({
			id: 'e3',
			list_id: 'other-list',
			resolution_status: 'ambiguous',
			resolution_candidates_json: JSON.stringify(['ci-a']),
			raw_text: 'cream',
			display_name: 'cream',
			quantity: 1,
			status: 'active',
			last_updated_revision: 0,
			updated_at: '2026-06-05T00:00:00.000Z'
		});

		await resolveAlias(LIST, 'e3', 'ci-a');

		expect(await db.mutation_queue.count()).toBe(0);
		expect((await db.list_entries.get('e3'))?.resolution_status).toBe('ambiguous');
	});

	it('is a no-op when the entry was renamed between the pre-read and the tx (stale raw_text)', async () => {
		// The real stored row already carries the NEW term ("creme"); the payload's
		// raw_text is captured from the pre-read. Simulate the rename by having ONLY
		// the first `get` (the pre-read) return the stale "cream" snapshot, so the
		// payload would carry "cream" while the tx re-read sees "creme".
		await seedAmbiguous('e4', 'creme', ['ci-a', 'ci-b']);
		const realGet = db.list_entries.get.bind(db.list_entries);
		// Dexie's `get` has overloads, so the mock impl is typed loosely; the runtime
		// behaviour (return the real row with a stale raw_text on the FIRST call) is
		// what the guard is being exercised against.
		const spy = vi.spyOn(db.list_entries, 'get').mockImplementationOnce((async (key: string) => {
			const row = await realGet(key);
			return row ? { ...row, raw_text: 'cream' } : row;
			// eslint-disable-next-line @typescript-eslint/no-explicit-any
		}) as any);

		await resolveAlias(LIST, 'e4', 'ci-a');

		// The tx-read sees raw_text "creme" ≠ payload's "cream" → guarded no-op: no
		// mutation queued and the entry stays ambiguous (never bound to a stale term).
		expect(await db.mutation_queue.count()).toBe(0);
		const entry = await db.list_entries.get('e4');
		expect(entry?.resolution_status).toBe('ambiguous');
		expect(entry?.canonical_item_id).toBeUndefined();
		spy.mockRestore();
	});

	it('is a no-op when the entry is no longer active (bought/skipped/deleted)', async () => {
		// The modal went stale: an SSE update marked the ambiguous entry `bought`
		// before Confirm. Resolving it would optimistically resurrect it and queue a
		// mutation the backend can only no-op, so the client must not enqueue at all.
		await db.list_entries.add({
			id: 'e5',
			list_id: LIST,
			resolution_status: 'ambiguous',
			resolution_candidates_json: JSON.stringify(['ci-a', 'ci-b']),
			raw_text: 'cream',
			display_name: 'cream',
			quantity: 1,
			status: 'bought',
			last_updated_revision: 0,
			updated_at: '2026-06-05T00:00:00.000Z'
		});

		await resolveAlias(LIST, 'e5', 'ci-a');

		expect(await db.mutation_queue.count()).toBe(0);
		const entry = await db.list_entries.get('e5');
		// Untouched: still ambiguous + bought, never bound.
		expect(entry?.resolution_status).toBe('ambiguous');
		expect(entry?.status).toBe('bought');
		expect(entry?.canonical_item_id).toBeUndefined();
	});
});

describe('setQuantity', () => {
	it('sets the absolute quantity locally and queues a set_quantity mutation', async () => {
		const id = await addItem(LIST, 'Apples');

		await setQuantity(LIST, id, 3);

		expect((await db.list_entries.get(id))?.quantity).toBe(3);
		const mut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'set_quantity'
		);
		expect(mut).toMatchObject({
			entity_id: id,
			status: 'pending',
			payload: { quantity: 3 }
		});
	});

	it('clamps below 1 to 1 (quantity never goes below 1)', async () => {
		const id = await addItem(LIST, 'Bread', 2);

		await setQuantity(LIST, id, 0);

		expect((await db.list_entries.get(id))?.quantity).toBe(1);
		const mut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'set_quantity'
		);
		expect(mut?.payload).toEqual({ quantity: 1 });
	});

	it('clamps above MAX_QUANTITY to the ceiling (never round-trips a value the server rejects)', async () => {
		// The UI disables `+` at MAX_QUANTITY, but a programmatic caller could exceed
		// it. Clamp locally so we never optimistically persist a value the backend
		// rejects with 422 (which would otherwise roll back on the next sync).
		const id = await addItem(LIST, 'Napkins', 2);

		await setQuantity(LIST, id, MAX_QUANTITY + 1);

		expect((await db.list_entries.get(id))?.quantity).toBe(MAX_QUANTITY);
		const mut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'set_quantity'
		);
		expect(mut?.payload).toEqual({ quantity: MAX_QUANTITY });
	});

	it('treats a non-finite quantity (NaN/Infinity) as 1, never persisting NaN', async () => {
		// A non-finite input must fall back to 1 (not poison the entry with NaN,
		// which would defeat the unchanged-value no-op check). On a quantity-2 entry
		// that means a real change to 1, enqueuing a set_quantity payload of 1.
		const id = await addItem(LIST, 'Olives', 2);

		await setQuantity(LIST, id, Number.NaN);

		expect((await db.list_entries.get(id))?.quantity).toBe(1);
		const nanMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'set_quantity'
		);
		expect(nanMut?.payload).toEqual({ quantity: 1 });

		// Infinity on an already-1 entry clamps to 1 and is therefore a no-op:
		// no NaN reaches Dexie and no redundant mutation is enqueued.
		const before = await db.mutation_queue.count();
		await setQuantity(LIST, id, Number.POSITIVE_INFINITY);
		expect((await db.list_entries.get(id))?.quantity).toBe(1);
		expect(await db.mutation_queue.count()).toBe(before);
	});

	it('is a no-op when the quantity is unchanged (no redundant mutation)', async () => {
		const id = await addItem(LIST, 'Eggs', 2);
		const before = await db.mutation_queue.count();

		await setQuantity(LIST, id, 2);

		expect(await db.mutation_queue.count()).toBe(before);
	});

	it('is a no-op on an unknown id', async () => {
		await setQuantity(LIST, 'does-not-exist', 5);
		expect(await db.mutation_queue.count()).toBe(0);
		expect(await db.list_entries.count()).toBe(0);
	});

	it('is a no-op for an entry on a different list', async () => {
		await db.list_entries.add({
			id: 'q1',
			list_id: 'other-list',
			resolution_status: 'unresolved',
			raw_text: 'milk',
			display_name: 'Milk',
			quantity: 1,
			status: 'active',
			last_updated_revision: 0,
			updated_at: '2026-06-05T00:00:00.000Z'
		});

		await setQuantity(LIST, 'q1', 4);

		expect(await db.mutation_queue.count()).toBe(0);
		expect((await db.list_entries.get('q1'))?.quantity).toBe(1);
	});

	it('does not resurrect a deleted entry (delete wins)', async () => {
		const id = await addItem(LIST, 'Cereal');
		await deleteItem(LIST, id);
		const before = await db.mutation_queue.count();

		await setQuantity(LIST, id, 5);

		// No quantity change, no extra mutation.
		const entry = await db.list_entries.get(id);
		expect(entry?.status).toBe('deleted');
		expect(entry?.quantity).toBe(1);
		expect(await db.mutation_queue.count()).toBe(before);
	});
});
