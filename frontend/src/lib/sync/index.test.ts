import { beforeEach, describe, expect, it, vi } from 'vitest';
import { db, type LocalMutation } from '$lib/db/dexie';
import { startSync, fetchSnapshot, notifyMutationEnqueued } from './index';
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

  it('stop() before the initial drain completes never opens SSE', async () => {
    await enqueue('e1');

    let started = false;
    // A fetch that resolves on the next microtask, so we can call stop() while
    // the bootstrap drain is still in flight.
    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }),
          { status: 200 }
        );
      }
      const sent = JSON.parse(String(init?.body));
      return new Response(
        JSON.stringify({
          client_mutation_id: sent.client_mutation_id,
          status: 'accepted',
          server_revision: 1,
          entity: serverEntity('e1', 'active', 1)
        } satisfies MutationResponse),
        { status: 200 }
      );
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, {
      fetchFn,
      sse: {
        eventSourceFactory: () => {
          started = true;
          return fakeSse();
        }
      }
    });
    handle.stop(); // tear down before the bootstrap chain resolves
    await handle.resync().catch(() => {}); // let the bootstrap settle

    expect(started).toBe(false); // gated by the cancellation flag
  });

  it('online during the in-flight initial drain runs only one sync', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    await enqueue('e1');

    let postCount = 0;
    let releaseFirstPost!: () => void;
    const firstPostGate = new Promise<void>((r) => {
      releaseFirstPost = r;
    });

    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }),
          { status: 200 }
        );
      }
      postCount += 1;
      if (postCount === 1) await firstPostGate; // hold the first drain open
      const sent = JSON.parse(String(init?.body));
      return new Response(
        JSON.stringify({
          client_mutation_id: sent.client_mutation_id,
          status: 'accepted',
          server_revision: postCount,
          entity: serverEntity(sent.entity_id, 'active', postCount)
        } satisfies MutationResponse),
        { status: 200 }
      );
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });

    // Fire an `online` event while the bootstrap drain is parked on firstPostGate.
    window.dispatchEvent(new Event('online'));
    releaseFirstPost(); // let the in-flight drain finish

    await handle.resync(); // drains chain serially; settles all queued syncs

    // The single pending row was POSTed exactly once — the online event chained
    // behind the bootstrap rather than running a second concurrent drain.
    expect(postCount).toBe(1);
    expect(await db.mutation_queue.where('status').equals('confirmed').count()).toBe(1);
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

  it('flushes immediately when a mutation is enqueued while running', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    let posts = 0;
    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }),
          { status: 200 }
        );
      }
      posts += 1;
      const sent = JSON.parse(String(init?.body));
      return new Response(
        JSON.stringify({
          client_mutation_id: sent.client_mutation_id,
          status: 'accepted',
          server_revision: posts,
          entity: serverEntity(sent.entity_id, 'active', posts)
        } satisfies MutationResponse),
        { status: 200 }
      );
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync(); // bootstrap settles (no pending rows yet)

    // Enqueue + signal (as the store does). The engine flushes without a resync.
    await enqueue('e1');
    notifyMutationEnqueued(LIST);
    await handle.resync(); // drains chain serially; settles the signalled flush

    expect(posts).toBe(1);
    expect(await db.mutation_queue.where('status').equals('confirmed').count()).toBe(1);
    handle.stop();
  });

  it('ignores enqueue signals for other lists and after stop()', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    let posts = 0;
    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }),
          { status: 200 }
        );
      }
      posts += 1;
      const sent = JSON.parse(String(init?.body));
      return new Response(
        JSON.stringify({
          client_mutation_id: sent.client_mutation_id,
          status: 'accepted',
          server_revision: posts,
          entity: serverEntity(sent.entity_id, 'active', posts)
        } satisfies MutationResponse),
        { status: 200 }
      );
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();
    await enqueue('e1');

    // A signal for a different list must not flush this engine.
    notifyMutationEnqueued('some-other-list');
    await handle.resync().catch(() => {});
    expect(posts).toBe(1); // only the resync drained it (the foreign signal didn't)

    // After teardown, signals are a no-op.
    await enqueue('e2');
    handle.stop();
    notifyMutationEnqueued(LIST);
    await new Promise((r) => setTimeout(r, 0));
    expect(await db.mutation_queue.where('status').equals('pending').count()).toBe(1); // e2 stays
  });
});

describe('runSync reconcile-on-failure', () => {
  /** Queues a mutation + its optimistic local row, like the store. */
  async function enqueueOp(
    entryId: string,
    operation: string,
    entryStatus: ServerListEntry['status']
  ): Promise<void> {
    await db.list_entries.put({
      id: entryId,
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: entryId,
      display_name: entryId,
      quantity: 1,
      status: entryStatus,
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.mutation_queue.add({
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
      retry_count: 0
    } as LocalMutation);
  }

  it('removes a phantom optimistic add_item after a 4xx reconcile snapshot', async () => {
    // meta already exists (revision unchanged by a rejection), so without the
    // reconcile flag the snapshot apply would short-circuit and leave the phantom.
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    await enqueueOp('phantom', 'add_item', 'active');

    let snapshotGets = 0;
    const fetchFn = vi.fn(async (url: string) => {
      if (url.startsWith('/api/list/')) {
        snapshotGets += 1;
        // Active-only snapshot at the SAME revision — the rejected add isn't in it.
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 5, entries: [] }),
          { status: 200 }
        );
      }
      // POST /api/mutations → terminal 4xx rejection.
      return new Response('item not allowed', { status: 422 });
    }) as unknown as typeof fetch;

    const failures: string[] = [];
    const handle = startSync(LIST, {
      fetchFn,
      onFailed: (f) => failures.push(f.operation_type),
      sse: { eventSourceFactory: () => fakeSse() }
    });
    await handle.resync();

    expect(failures).toEqual(['add_item']); // surfaced to the caller
    expect(snapshotGets).toBe(1); // reconcile GET fired exactly once
    expect(await db.list_entries.get('phantom')).toBeUndefined(); // phantom removed
    expect((await db.mutation_queue.get(1))?.status).toBe('failed');
    handle.stop();
  });

  it('reverts a rejected mark_bought to the server-authoritative active row', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    // Optimistically marked bought locally; the server will reject it.
    await enqueueOp('item-1', 'mark_bought', 'bought');

    const fetchFn = vi.fn(async (url: string) => {
      if (url.startsWith('/api/list/')) {
        // Server still has the entry ACTIVE (the bought never applied).
        return new Response(
          JSON.stringify({
            id: LIST,
            name: 'L',
            current_revision: 5,
            entries: [
              {
                id: 'item-1',
                canonical_item_id: null,
                resolution_status: 'unresolved',
                raw_text: 'item-1',
                display_name: 'item-1',
                quantity: 1,
                status: 'active',
                last_updated_revision: 5,
                updated_at: '2026-06-03T00:00:00.000Z'
              }
            ]
          }),
          { status: 200 }
        );
      }
      return new Response('conflict', { status: 409 });
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();

    // Optimistic 'bought' is reverted to the snapshot's authoritative 'active'.
    expect((await db.list_entries.get('item-1'))?.status).toBe('active');
    handle.stop();
  });

  it('removes a rejected add whose row drifted to bought before draining', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    // Offline add, then marked bought before the add drained → row is `bought`,
    // but the failing mutation is the add_item.
    await enqueueOp('drifted', 'add_item', 'bought');

    const fetchFn = vi.fn(async (url: string) => {
      if (url.startsWith('/api/list/')) {
        // Active-only snapshot — the rejected add isn't in it.
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 5, entries: [] }),
          { status: 200 }
        );
      }
      return new Response('item not allowed', { status: 422 });
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();

    // The bogus add entity is gone even though it was `bought` (not active).
    expect(await db.list_entries.get('drifted')).toBeUndefined();
    handle.stop();
  });

  it('removes a rejected add whose row drifted to skipped before draining', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    await enqueueOp('drifted-skip', 'add_item', 'skipped');

    const fetchFn = vi.fn(async (url: string) => {
      if (url.startsWith('/api/list/')) {
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 5, entries: [] }),
          { status: 200 }
        );
      }
      return new Response('item not allowed', { status: 422 });
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();

    expect(await db.list_entries.get('drifted-skip')).toBeUndefined();
    handle.stop();
  });

  it('preserves a legitimate bought row while removing a different failed add', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    // A legitimately-bought item: its add_item already CONFIRMED (not in failed set).
    await db.list_entries.put({
      id: 'kept-bought',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'kept-bought',
      display_name: 'kept-bought',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 4,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    // A separate failing add whose row drifted to bought.
    await enqueueOp('bad-add', 'add_item', 'bought');

    const fetchFn = vi.fn(async (url: string) => {
      if (url.startsWith('/api/list/')) {
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 5, entries: [] }),
          { status: 200 }
        );
      }
      return new Response('item not allowed', { status: 422 });
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();

    expect((await db.list_entries.get('kept-bought'))?.status).toBe('bought'); // preserved
    expect(await db.list_entries.get('bad-add')).toBeUndefined(); // failed add removed
    handle.stop();
  });

  it('does not fetch a reconcile snapshot when no mutation fails (meta exists)', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    await enqueue('ok-1');

    let snapshotGets = 0;
    let rev = 0;
    const fetchFn = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/list/')) {
        snapshotGets += 1;
        return new Response(
          JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }),
          { status: 200 }
        );
      }
      rev += 1;
      const sent = JSON.parse(String(init?.body));
      return new Response(
        JSON.stringify({
          client_mutation_id: sent.client_mutation_id,
          status: 'accepted',
          server_revision: rev,
          entity: serverEntity(sent.entity_id, 'active', rev)
        } satisfies MutationResponse),
        { status: 200 }
      );
    }) as unknown as typeof fetch;

    const handle = startSync(LIST, { fetchFn, sse: { eventSourceFactory: () => fakeSse() } });
    await handle.resync();

    expect(snapshotGets).toBe(0); // meta existed → no seed GET; no failure → no reconcile GET
    handle.stop();
  });
});

describe('fetchSnapshot reconcile flag', () => {
  it('bypasses the revision short-circuit but still preserves a pending add row', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    // A rejected add (failed) that must be reconciled away...
    await db.list_entries.put({
      id: 'rejected',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'rejected',
      display_name: 'rejected',
      quantity: 1,
      status: 'active',
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.mutation_queue.add({
      client_mutation_id: 'cmid-rejected',
      device_id: 'dev-1',
      list_id: LIST,
      entity_type: 'list_entry',
      entity_id: 'rejected',
      operation_type: 'add_item',
      payload: {},
      base_revision: 0,
      created_at_client: '2026-06-03T00:00:00.000Z',
      status: 'failed',
      retry_count: 0
    } as LocalMutation);
    // ...alongside a still-pending add that must be PRESERVED through reconcile.
    await db.list_entries.put({
      id: 'still-pending',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'still-pending',
      display_name: 'still-pending',
      quantity: 1,
      status: 'active',
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.mutation_queue.add({
      client_mutation_id: 'cmid-pending',
      device_id: 'dev-1',
      list_id: LIST,
      entity_type: 'list_entry',
      entity_id: 'still-pending',
      operation_type: 'add_item',
      payload: {},
      base_revision: 0,
      created_at_client: '2026-06-03T00:00:00.000Z',
      status: 'pending',
      retry_count: 0
    } as LocalMutation);

    const snapshot: ListSnapshot = { id: LIST, name: 'L', current_revision: 5, entries: [] };
    const fetchFn = vi.fn(async () =>
      new Response(JSON.stringify(snapshot), { status: 200 })
    ) as unknown as typeof fetch;

    // Same revision (5 === 5): without reconcile this is a no-op short-circuit.
    await fetchSnapshot(LIST, fetchFn, { reconcile: true });

    expect(await db.list_entries.get('rejected')).toBeUndefined(); // failed → removed
    expect(await db.list_entries.get('still-pending')).toBeDefined(); // pending → preserved
  });
});
