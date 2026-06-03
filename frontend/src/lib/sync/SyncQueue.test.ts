import { beforeEach, describe, expect, it, vi } from 'vitest';
import { db, type LocalMutation, type LocalListEntry } from '$lib/db/dexie';
import { drainQueue, MAX_RETRIES, type FailedMutation } from './SyncQueue';
import type { MutationResponse, ServerListEntry } from './types';

const LIST = 'list-1';

beforeEach(async () => {
  await db.delete();
  await db.open();
});

/** Seeds a pending mutation + its optimistic local entry, mimicking the store. */
async function enqueue(
  entryId: string,
  operation: string,
  overrides: Partial<LocalMutation> = {}
): Promise<number> {
  const entry: LocalListEntry = {
    id: entryId,
    list_id: LIST,
    resolution_status: 'unresolved',
    raw_text: 'Milk',
    display_name: 'Milk',
    quantity: 1,
    status: 'active',
    last_updated_revision: 0,
    updated_at: '2026-06-03T00:00:00.000Z'
  };
  await db.list_entries.put(entry);
  return (await db.mutation_queue.add({
    client_mutation_id: `cmid-${entryId}-${operation}`,
    device_id: 'dev-1',
    list_id: LIST,
    entity_type: 'list_entry',
    entity_id: entryId,
    operation_type: operation,
    payload: {},
    base_revision: 0,
    created_at_client: '2026-06-03T00:00:00.000Z',
    status: 'pending',
    retry_count: 0,
    ...overrides
  })) as number;
}

/** Builds a server entity for `entryId` with the given status. */
function serverEntity(entryId: string, status: ServerListEntry['status'], rev: number): ServerListEntry {
  return {
    id: entryId,
    list_id: LIST,
    canonical_item_id: null,
    resolution_status: 'unresolved',
    raw_text: 'Milk',
    display_name: 'Milk',
    quantity: 1,
    status,
    bought_by: null,
    bought_at: null,
    category_id: null,
    last_updated_revision: rev,
    updated_at: '2026-06-03T00:01:00.000Z'
  };
}

function okResponse(body: MutationResponse): Response {
  return new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } });
}

describe('drainQueue', () => {
  it('drains pending mutations in creation order and confirms them', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const id1 = await enqueue('e1', 'add_item');
    const id2 = await enqueue('e2', 'add_item');

    const seen: string[] = [];
    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      const sent = JSON.parse(String(init?.body));
      seen.push(sent.entity_id);
      const rev = seen.length;
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: rev,
        entity: serverEntity(sent.entity_id, 'active', rev)
      });
    }) as unknown as typeof fetch;

    const result = await drainQueue(LIST, { fetchFn });

    expect(seen).toEqual(['e1', 'e2']); // ordered by Dexie id
    expect(result.confirmed).toBe(2);
    expect(result.failed).toHaveLength(0);

    expect((await db.mutation_queue.get(id1))?.status).toBe('confirmed');
    expect((await db.mutation_queue.get(id2))?.status).toBe('confirmed');
    // Revision advanced to the last server_revision.
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(2);
  });

  it('replaces the local entry with the server entity (server authoritative)', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    await enqueue('e1', 'mark_bought');

    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      const sent = JSON.parse(String(init?.body));
      const entity = serverEntity('e1', 'bought', 5);
      entity.bought_by = 'user-9';
      entity.bought_at = '2026-06-03T00:02:00.000Z';
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 5,
        entity
      });
    }) as unknown as typeof fetch;

    await drainQueue(LIST, { fetchFn });

    const entry = await db.list_entries.get('e1');
    expect(entry?.status).toBe('bought');
    expect(entry?.bought_by).toBe('user-9');
    expect(entry?.last_updated_revision).toBe(5);
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(5);
  });

  it('retries network errors with exponential backoff up to MAX_RETRIES', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const id = await enqueue('e1', 'add_item');

    let calls = 0;
    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      calls += 1;
      if (calls <= 2) throw new TypeError('Failed to fetch'); // transient
      const sent = JSON.parse(String(init?.body));
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 1,
        entity: serverEntity('e1', 'active', 1)
      });
    }) as unknown as typeof fetch;

    const delays: number[] = [];
    const sleep = vi.fn(async (ms: number) => {
      delays.push(ms);
    });

    const result = await drainQueue(LIST, { fetchFn, sleep });

    expect(calls).toBe(3); // 2 failures + 1 success
    expect(delays).toEqual([500, 1000]); // exponential backoff
    expect(result.confirmed).toBe(1);
    expect((await db.mutation_queue.get(id))?.status).toBe('confirmed');
    expect((await db.mutation_queue.get(id))?.retry_count).toBe(2);
  });

  it('stops draining and leaves the mutation pending after exhausting retries', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const id1 = await enqueue('e1', 'add_item');
    const id2 = await enqueue('e2', 'add_item');

    const fetchFn = vi.fn(async () => {
      throw new TypeError('offline');
    }) as unknown as typeof fetch;

    const result = await drainQueue(LIST, { fetchFn, sleep: async () => {} });

    expect(result.confirmed).toBe(0);
    expect(result.failed).toHaveLength(0); // network failure is not a rejection
    // First mutation tried MAX_RETRIES+1 times then bailed; both stay pending so
    // the next reconnect resumes in order.
    expect((await db.mutation_queue.get(id1))?.status).toBe('pending');
    expect((await db.mutation_queue.get(id1))?.retry_count).toBe(MAX_RETRIES + 1);
    expect((await db.mutation_queue.get(id2))?.status).toBe('pending');
    expect((fetchFn as unknown as ReturnType<typeof vi.fn>).mock.calls).toHaveLength(MAX_RETRIES + 1);
  });

  it('marks a 4xx-rejected mutation failed, surfaces it, and continues', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const id1 = await enqueue('e1', 'add_item');
    const id2 = await enqueue('e2', 'add_item');

    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      const sent = JSON.parse(String(init?.body));
      if (sent.entity_id === 'e1') {
        return new Response('base_revision too old', { status: 409 });
      }
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 1,
        entity: serverEntity('e2', 'active', 1)
      });
    }) as unknown as typeof fetch;

    const surfaced: FailedMutation[] = [];
    const result = await drainQueue(LIST, { fetchFn, onFailed: (f) => surfaced.push(f) });

    expect((await db.mutation_queue.get(id1))?.status).toBe('failed');
    expect((await db.mutation_queue.get(id2))?.status).toBe('confirmed'); // drain continued
    expect(result.confirmed).toBe(1);
    expect(result.failed).toHaveLength(1);
    expect(result.failed[0]).toMatchObject({ entity_id: 'e1', status: 409 });
    expect(surfaced).toHaveLength(1);
    expect(surfaced[0].message).toContain('base_revision too old');
  });

  it('treats a 5xx as retryable, not a rejection', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const id = await enqueue('e1', 'add_item');

    let calls = 0;
    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      calls += 1;
      if (calls === 1) return new Response('boom', { status: 503 });
      const sent = JSON.parse(String(init?.body));
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 1,
        entity: serverEntity('e1', 'active', 1)
      });
    }) as unknown as typeof fetch;

    const result = await drainQueue(LIST, { fetchFn, sleep: async () => {} });

    expect(calls).toBe(2);
    expect(result.confirmed).toBe(1);
    expect((await db.mutation_queue.get(id))?.status).toBe('confirmed');
  });

  it('does not roll the revision backwards on an out-of-order response', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 10 });
    await enqueue('e1', 'add_item');

    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      const sent = JSON.parse(String(init?.body));
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 4, // lower than the local baseline
        entity: serverEntity('e1', 'active', 4)
      });
    }) as unknown as typeof fetch;

    await drainQueue(LIST, { fetchFn });

    expect((await db.list_meta.get(LIST))?.current_revision).toBe(10);
  });

  it('only drains pending mutations for the requested list', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    await enqueue('e1', 'add_item');
    await enqueue('e2', 'add_item', { status: 'confirmed' }); // already done
    await db.mutation_queue.add({
      client_mutation_id: 'other',
      device_id: 'dev-1',
      list_id: 'other-list',
      entity_type: 'list_entry',
      entity_id: 'x',
      operation_type: 'add_item',
      payload: {},
      base_revision: 0,
      created_at_client: '2026-06-03T00:00:00.000Z',
      status: 'pending',
      retry_count: 0
    });

    const fetchFn = vi.fn(async (_url, init?: RequestInit) => {
      const sent = JSON.parse(String(init?.body));
      return okResponse({
        client_mutation_id: sent.client_mutation_id,
        status: 'accepted',
        server_revision: 1,
        entity: serverEntity(sent.entity_id, 'active', 1)
      });
    }) as unknown as typeof fetch;

    const result = await drainQueue(LIST, { fetchFn });

    expect(result.confirmed).toBe(1); // only e1
    expect((fetchFn as unknown as ReturnType<typeof vi.fn>).mock.calls).toHaveLength(1);
  });
});
