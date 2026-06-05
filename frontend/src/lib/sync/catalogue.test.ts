import { beforeEach, describe, expect, it, vi } from 'vitest';
import { db } from '$lib/db/dexie';
import { fetchCatalogue } from './catalogue';
import { applyMutationEvent, applySnapshotEvent } from './SseClient';
import { sortByStoreOrder, UNCATEGORISED_ID } from '$lib/shopping/sortByStoreOrder';
import type { ListSnapshot, ServerListEntry, SseMutationData } from './types';

const LIST = 'list-1';

beforeEach(async () => {
  await db.delete();
  await db.open();
});

/** The `GET /api/items` envelope our backend now returns (ItemsResponse). */
function itemsEnvelope() {
  return {
    items: [
      { id: 'ci-milk', category_id: 'dairy', name_en: 'Milk' },
      { id: 'ci-apple', category_id: 'produce', name_en: 'Apple' }
    ],
    categories: [
      { id: 'produce', name_en: 'Produce', name_es: 'Frutas', icon: 'apple', default_order: 1 },
      { id: 'dairy', name_en: 'Dairy', name_es: 'Lácteos', icon: 'milk', default_order: 2 }
    ],
    store_category_orders: [
      // Tesco: dairy walked BEFORE produce (overrides default_order).
      { id: 'sco-tesco-dairy', store: 'tesco', category_id: 'dairy', display_order: 1 },
      { id: 'sco-tesco-produce', store: 'tesco', category_id: 'produce', display_order: 2 }
    ]
  };
}

function envelopeResponse(): Response {
  return new Response(JSON.stringify(itemsEnvelope()), { status: 200 });
}

function serverEntity(id: string, canonicalItemId: string | null, rev: number): ServerListEntry {
  return {
    id,
    list_id: LIST,
    canonical_item_id: canonicalItemId,
    resolution_status: canonicalItemId ? 'resolved' : 'unresolved',
    raw_text: id,
    display_name: id,
    quantity: 1,
    status: 'active',
    bought_by: null,
    bought_at: null,
    category_id: null, // server does not send category_id — client derives it
    last_updated_revision: rev,
    updated_at: '2026-06-03T00:00:00.000Z'
  };
}

describe('fetchCatalogue', () => {
  it('writes categories, store orders, and the item→category map into Dexie', async () => {
    const fetchFn = vi.fn(async () => envelopeResponse()) as unknown as typeof fetch;

    const ok = await fetchCatalogue(fetchFn);

    expect(ok).toBe(true);
    expect(fetchFn).toHaveBeenCalledWith('/api/items');
    expect(await db.categories.count()).toBe(2);
    expect(await db.store_category_orders.count()).toBe(2);
    expect(await db.item_categories.count()).toBe(2);
    expect((await db.item_categories.get('ci-milk'))?.category_id).toBe('dairy');
    // The item's English name is cached too, so the P3-T3 ambiguity modal can
    // label candidate ids offline.
    expect((await db.item_categories.get('ci-milk'))?.name_en).toBe('Milk');
  });

  it('returns false and keeps the cache when offline', async () => {
    // Seed a prior good cache, then a failing fetch must not wipe it.
    await db.categories.put({
      id: 'dairy',
      name_en: 'Dairy',
      name_es: 'Lácteos',
      icon: 'milk',
      default_order: 2
    });
    const fetchFn = vi.fn(async () => {
      throw new TypeError('offline');
    }) as unknown as typeof fetch;

    const ok = await fetchCatalogue(fetchFn);

    expect(ok).toBe(false);
    expect(await db.categories.count()).toBe(1); // cache untouched
  });

  it('returns false and keeps the cache for a non-OK response', async () => {
    await db.item_categories.put({ canonical_item_id: 'ci-milk', category_id: 'dairy' });
    const fetchFn = vi.fn(
      async () => new Response('nope', { status: 500 })
    ) as unknown as typeof fetch;

    expect(await fetchCatalogue(fetchFn)).toBe(false);
    expect(await db.item_categories.count()).toBe(1);
  });

  it('rejects a malformed (non-envelope) body without clobbering the cache', async () => {
    await db.item_categories.put({ canonical_item_id: 'ci-milk', category_id: 'dairy' });
    // A bare array (the OLD /api/items shape) is no longer valid.
    const fetchFn = vi.fn(
      async () => new Response(JSON.stringify([{ id: 'x' }]), { status: 200 })
    ) as unknown as typeof fetch;

    expect(await fetchCatalogue(fetchFn)).toBe(false);
    expect(await db.item_categories.count()).toBe(1);
  });

  it('backfills category_id on already-stored entries (catalogue AFTER entries)', async () => {
    // Entry synced first (no category, but a known canonical_item_id).
    await db.list_entries.put({
      id: 'e1',
      list_id: LIST,
      canonical_item_id: 'ci-milk',
      resolution_status: 'resolved',
      raw_text: 'milk',
      display_name: 'milk',
      quantity: 1,
      status: 'active',
      last_updated_revision: 1,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    const fetchFn = vi.fn(async () => envelopeResponse()) as unknown as typeof fetch;
    await fetchCatalogue(fetchFn);

    expect((await db.list_entries.get('e1'))?.category_id).toBe('dairy');
  });

  it('never overwrites a category already present on an entry', async () => {
    await db.list_entries.put({
      id: 'e1',
      list_id: LIST,
      canonical_item_id: 'ci-milk',
      resolution_status: 'resolved',
      raw_text: 'milk',
      display_name: 'milk',
      quantity: 1,
      status: 'active',
      category_id: 'frozen', // already set — must be preserved
      last_updated_revision: 1,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    await fetchCatalogue(vi.fn(async () => envelopeResponse()) as unknown as typeof fetch);

    expect((await db.list_entries.get('e1'))?.category_id).toBe('frozen');
  });
});

describe('sync apply → category resolution', () => {
  it('resolves category_id from the cache when a mutation event arrives (catalogue FIRST)', async () => {
    // Catalogue cached first, then the entry streams in via SSE.
    await fetchCatalogue(vi.fn(async () => envelopeResponse()) as unknown as typeof fetch);

    const data: SseMutationData = {
      server_revision: 5,
      entity: serverEntity('e1', 'ci-apple', 5)
    };
    await applyMutationEvent(data);

    expect((await db.list_entries.get('e1'))?.category_id).toBe('produce');
  });

  it('leaves an unresolved entry uncategorised → it buckets under "Other"', async () => {
    await fetchCatalogue(vi.fn(async () => envelopeResponse()) as unknown as typeof fetch);

    // No canonical_item_id → no category. (Also covers a cache miss implicitly.)
    await applyMutationEvent({
      server_revision: 3,
      entity: serverEntity('e1', null, 3)
    });

    const entry = await db.list_entries.get('e1');
    expect(entry?.category_id).toBeUndefined();

    const groups = sortByStoreOrder(
      [entry!],
      await db.store_category_orders.toArray(),
      'tesco',
      await db.categories.toArray()
    );
    expect(groups).toHaveLength(1);
    expect(groups[0].category_id).toBe(UNCATEGORISED_ID);
  });

  it('groups a synced entry under the selected store order (end-to-end)', async () => {
    await fetchCatalogue(vi.fn(async () => envelopeResponse()) as unknown as typeof fetch);

    const snapshot: ListSnapshot = {
      id: LIST,
      name: 'Groceries',
      current_revision: 4,
      entries: [serverEntity('e-apple', 'ci-apple', 4), serverEntity('e-milk', 'ci-milk', 4)]
    };
    await applySnapshotEvent(LIST, snapshot);

    const entries = await db.list_entries.where('list_id').equals(LIST).toArray();
    // Tesco curates dairy (order 1) BEFORE produce (order 2), so dairy leads.
    const groups = sortByStoreOrder(
      entries,
      await db.store_category_orders.toArray(),
      'tesco',
      await db.categories.toArray()
    );
    expect(groups.map((g) => g.category_id)).toEqual(['dairy', 'produce']);

    // A different store falls back to category default_order (produce=1, dairy=2).
    const aldiGroups = sortByStoreOrder(
      entries,
      await db.store_category_orders.toArray(),
      'aldi',
      await db.categories.toArray()
    );
    expect(aldiGroups.map((g) => g.category_id)).toEqual(['produce', 'dairy']);
  });
});
