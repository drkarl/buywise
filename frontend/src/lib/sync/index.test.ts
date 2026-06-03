import { beforeEach, describe, expect, it, vi } from 'vitest';
import { db, type LocalMutation } from '$lib/db/dexie';
import { startSync, fetchSnapshot } from './index';
import type { ListSnapshot, MutationResponse, ServerListEntry } from './types';
import type { EventSourceLike } from './SseClient';

const LIST = 'list-1';

beforeEach(async () => {
  await db.delete();
  await db.open();
});

function serverEntity(id: string, status: ServerListEntry['status'], rev: number): ServerListEntry {
  return {
    id,
    list_id: LIST,
    canonical_item_id: null,
    resolution_status: 'unresolved',
    raw_text: id,
    display_name: id,
    quantity: 1,
    status,
    bought_by: null,
    bought_at: null,
    category_id: null,
    last_updated_revision: rev,
    updated_at: '2026-06-03T00:00:00.000Z'
  };
}

async function enqueue(entryId: string): Promise<number> {
  await db.list_entries.put({
    id: entryId,
    list_id: LIST,
    resolution_status: 'unresolved',
    raw_text: entryId,
    display_name: entryId,
    quantity: 1,
    status: 'active',
    last_updated_revision: 0,
    updated_at: '2026-06-03T00:00:00.000Z'
  });
  return (await db.mutation_queue.add({
    client_mutation_id: `cmid-${entryId}`,
    device_id: 'dev-1',
    list_id: LIST,
    entity_type: 'list_entry',
    entity_id: entryId,
    operation_type: 'add_item',
    payload: {},
    base_revision: 0,
    created_at_client: '2026-06-03T00:00:00.000Z',
    status: 'pending',
    retry_count: 0
  } as LocalMutation)) as number;
}

/** No-op SSE source so startSync's stream half never touches the network. */
function fakeSse(): EventSourceLike {
  return {
    addEventListener() {},
    close() {},
    onerror: null,
    onopen: null
  };
}

describe('fetchSnapshot', () => {
  it('replaces local entries from GET /api/list/:id and sets the revision', async () => {
    await db.list_entries.put({
      id: 'stale',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'x',
      display_name: 'x',
      quantity: 1,
      status: 'active',
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    const snapshot: ListSnapshot = {
      id: LIST,
      name: 'Groceries',
      current_revision: 8,
      entries: [serverEntity('a', 'active', 8)]
    };
    const fetchFn = vi.fn(async () =>
      new Response(JSON.stringify(snapshot), { status: 200 })
    ) as unknown as typeof fetch;

    const result = await fetchSnapshot(LIST, fetchFn);

    expect(result?.current_revision).toBe(8);
    const ids = (await db.list_entries.where('list_id').equals(LIST).toArray()).map((e) => e.id);
    expect(ids).toEqual(['a']);
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(8);
  });

  it('returns null and keeps local state when offline', async () => {
    const fetchFn = vi.fn(async () => {
      throw new TypeError('offline');
    }) as unknown as typeof fetch;
    expect(await fetchSnapshot(LIST, fetchFn)).toBeNull();
  });
});

describe('startSync', () => {
  it('seeds from a snapshot, drains the queue, and converges to the server revision', async () => {
    // No local meta → startSync fetches the snapshot first, then drains.
    await enqueue('e1');

    const snapshot: ListSnapshot = {
      id: LIST,
      name: 'Groceries',
      current_revision: 0,
      entries: []
    };

    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        return new Response(JSON.stringify(snapshot), { status: 200 });
      }
      // POST /api/mutations
      const sent = JSON.parse(String(init?.body));
      const body: MutationResponse = {
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 1,
        entity: serverEntity('e1', 'active', 1)
      };
      return new Response(JSON.stringify(body), { status: 200 });
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, {
      fetchFn,
      sse: { eventSourceFactory: () => fakeSse() }
    });
    await handle.resync(); // awaits bootstrap + a drain pass

    expect((await db.mutation_queue.where('status').equals('confirmed').count())).toBe(1);
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(1);
    handle.stop();
  });

  it('resync re-drains newly queued mutations (online event path)', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });

    let rev = 0;
    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        return new Response(JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }), {
          status: 200
        });
      }
      const sent = JSON.parse(String(init?.body));
      rev += 1;
      const body: MutationResponse = {
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: rev,
        entity: serverEntity(sent.entity_id, 'active', rev)
      };
      return new Response(JSON.stringify(body), { status: 200 });
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();

    // A mutation is queued while online; a resync flushes it.
    await enqueue('e2');
    await handle.resync();

    expect((await db.mutation_queue.where('status').equals('confirmed').count())).toBe(1);
    handle.stop();
  });
});
