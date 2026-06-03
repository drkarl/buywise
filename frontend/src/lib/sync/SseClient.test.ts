import { beforeEach, describe, expect, it, vi } from 'vitest';
import { db, type LocalListEntry, type LocalMutation } from '$lib/db/dexie';
import {
  SseClient,
  applyMutationEvent,
  applySnapshotEvent,
  applyHeartbeat,
  backoffFor,
  type EventSourceLike,
  type SseClientOptions
} from './SseClient';
import type { ServerListEntry, SseMutationData, SseSnapshotData } from './types';

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

/** Controllable fake EventSource that records the URL it was opened with. */
class FakeEventSource implements EventSourceLike {
  static instances: FakeEventSource[] = [];
  url: string;
  closed = false;
  onerror: ((event: Event) => void) | null = null;
  onopen: ((event: Event) => void) | null = null;
  private listeners = new Map<string, (event: MessageEvent) => void>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, listener: (event: MessageEvent) => void): void {
    this.listeners.set(type, listener);
  }
  close(): void {
    this.closed = true;
  }
  emit(type: string, data: unknown): void {
    this.listeners.get(type)?.({ data: JSON.stringify(data) } as MessageEvent);
  }
  open(): void {
    this.onopen?.(new Event('open'));
  }
  fail(): void {
    this.onerror?.(new Event('error'));
  }
}

describe('event appliers', () => {
  it('applyMutationEvent upserts the entity and advances the revision', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 3 });
    const data: SseMutationData = { server_revision: 7, entity: serverEntity('e1', 'bought', 7) };

    await applyMutationEvent(data);

    expect((await db.list_entries.get('e1'))?.status).toBe('bought');
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(7);
  });

  it('applyMutationEvent never rolls the revision backwards', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 9 });
    await applyMutationEvent({ server_revision: 4, entity: serverEntity('e1', 'active', 4) });
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(9);
  });

  it('applyMutationEvent does not let an older entity overwrite a newer entry', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 1 });
    // A newer SSE event (delete at rev 3) applied first.
    await applyMutationEvent({ server_revision: 3, entity: serverEntity('e1', 'deleted', 3) });
    expect((await db.list_entries.get('e1'))?.status).toBe('deleted');

    // A delayed lower-revision entity (bought at rev 2) for the same entry must
    // NOT clobber the newer row, even though meta is already at 3.
    await applyMutationEvent({ server_revision: 2, entity: serverEntity('e1', 'bought', 2) });
    expect((await db.list_entries.get('e1'))?.status).toBe('deleted'); // unchanged
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(3);
  });

  it('applyMutationEvent applies an equal-or-higher revision entity', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 2 });
    await applyMutationEvent({ server_revision: 2, entity: serverEntity('e1', 'active', 2) });
    // Equal per-entry revision still applies (the LWW server reissued the row).
    await applyMutationEvent({ server_revision: 3, entity: serverEntity('e1', 'bought', 2) });
    expect((await db.list_entries.get('e1'))?.status).toBe('bought');
  });

  // The server's snapshot EntryView OMITS `list_id` and is active-only, so the
  // snapshot factory mirrors that: no `list_id` field; the applier stamps it.
  function snapshotEntity(
    id: string,
    status: ServerListEntry['status'],
    rev: number
  ): Omit<ServerListEntry, 'list_id'> {
    const { list_id: _omit, ...rest } = serverEntity(id, status, rev);
    void _omit;
    return rest;
  }

  it('applySnapshotEvent stamps list_id and upserts the active set', async () => {
    const stale: LocalListEntry = {
      id: 'old',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'stale',
      display_name: 'stale',
      quantity: 1,
      status: 'active',
      last_updated_revision: 1,
      updated_at: '2026-06-03T00:00:00.000Z'
    };
    await db.list_entries.put(stale);
    // An entry from a different list must NOT be touched by the snapshot.
    await db.list_entries.put({ ...stale, id: 'other', list_id: 'list-2' });
    await db.list_meta.put({ id: LIST, name: 'Old name', current_revision: 1 });

    const snapshot = {
      id: LIST,
      name: 'Fresh name',
      current_revision: 42,
      entries: [snapshotEntity('a', 'active', 40), snapshotEntity('b', 'active', 41)]
    } as unknown as SseSnapshotData;
    await applySnapshotEvent(LIST, snapshot);

    const rows = await db.list_entries.where('list_id').equals(LIST).toArray();
    const ids = rows.map((e) => e.id).sort();
    expect(ids).toEqual(['a', 'b']); // 'old' active row gone; snapshot actives in
    // Every snapshot row carries the stamped list_id (server omitted it).
    expect(rows.every((e) => e.list_id === LIST)).toBe(true);
    expect(await db.list_entries.get('other')).toBeDefined(); // other list untouched
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(42);
    expect((await db.list_meta.get(LIST))?.name).toBe('Fresh name');
  });

  it('applySnapshotEvent preserves local bought/skipped rows (active-only endpoint)', async () => {
    // A previously-bought row lives only locally — the active-only snapshot can't
    // enumerate it, so it must survive the snapshot apply (basket history kept).
    await db.list_entries.put({
      id: 'bought-1',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'milk',
      display_name: 'milk',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 5,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    // A stale local active row that the snapshot no longer lists must be removed.
    await db.list_entries.put({
      id: 'gone',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'gone',
      display_name: 'gone',
      quantity: 1,
      status: 'active',
      last_updated_revision: 3,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });

    await applySnapshotEvent(LIST, {
      id: LIST,
      name: 'L',
      current_revision: 10,
      entries: [snapshotEntity('still-active', 'active', 10)]
    } as unknown as SseSnapshotData);

    expect((await db.list_entries.get('bought-1'))?.status).toBe('bought'); // preserved
    expect(await db.list_entries.get('gone')).toBeUndefined(); // stale active removed
    expect(await db.list_entries.get('still-active')).toBeDefined(); // snapshot active in
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(10);
  });

  /** Seeds an optimistic active row + a queued mutation for it, like the store. */
  async function queueOptimisticAdd(
    entryId: string,
    status: LocalMutation['status']
  ): Promise<void> {
    await db.list_entries.put({
      id: entryId,
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: entryId,
      display_name: entryId,
      quantity: 1,
      status: 'active',
      last_updated_revision: 0, // optimistic insert; server hasn't assigned one
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.mutation_queue.add({
      client_mutation_id: `cmid-${entryId}`,
      device_id: 'dev-1',
      list_id: LIST,
      entity_type: 'list_entry',
      entity_id: entryId,
      operation_type: 'add_item',
      payload: {},
      base_revision: 0,
      created_at_client: '2026-06-03T00:00:00.000Z',
      status,
      retry_count: 0
    } as LocalMutation);
  }

  it('applySnapshotEvent preserves an active row backing a pending mutation', async () => {
    // Optimistic offline add: row exists locally, mutation still pending, so the
    // server snapshot cannot contain its id yet. It must NOT be deleted.
    await queueOptimisticAdd('opt-1', 'pending');
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });

    await applySnapshotEvent(LIST, {
      id: LIST,
      name: 'L',
      current_revision: 4,
      entries: [snapshotEntity('server-a', 'active', 4)]
    } as unknown as SseSnapshotData);

    expect(await db.list_entries.get('opt-1')).toBeDefined(); // preserved (pending)
    expect(await db.list_entries.get('server-a')).toBeDefined(); // snapshot active in
  });

  it('applySnapshotEvent deletes an absent active row with only a confirmed mutation', async () => {
    // A confirmed mutation means the server already knows the entity; if the
    // active-only snapshot omits it, another device bought/deleted it → delete.
    await queueOptimisticAdd('done-1', 'confirmed');
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });

    await applySnapshotEvent(LIST, {
      id: LIST,
      name: 'L',
      current_revision: 4,
      entries: []
    } as unknown as SseSnapshotData);

    expect(await db.list_entries.get('done-1')).toBeUndefined();
  });

  it('applySnapshotEvent deletes an absent active row backing a failed mutation', async () => {
    // A server-rejected mutation is terminal (surfaced via onFailed); it must not
    // pin a phantom active row forever.
    await queueOptimisticAdd('rej-1', 'failed');
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });

    await applySnapshotEvent(LIST, {
      id: LIST,
      name: 'L',
      current_revision: 4,
      entries: []
    } as unknown as SseSnapshotData);

    expect(await db.list_entries.get('rej-1')).toBeUndefined();
  });

  it('applySnapshotEvent removeEntityIds deletes a row in ANY status', async () => {
    // A rejected add whose row drifted to `bought` (active-scoped delete misses it).
    await db.list_entries.put({
      id: 'force-del',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'force-del',
      display_name: 'force-del',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    // A legit bought row that is NOT in removeEntityIds must survive.
    await db.list_entries.put({
      id: 'keep-bought',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'keep-bought',
      display_name: 'keep-bought',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });

    await applySnapshotEvent(
      LIST,
      { id: LIST, name: 'L', current_revision: 4, entries: [] } as unknown as SseSnapshotData,
      { removeEntityIds: ['force-del'] }
    );

    expect(await db.list_entries.get('force-del')).toBeUndefined(); // forced delete
    expect((await db.list_entries.get('keep-bought'))?.status).toBe('bought'); // preserved
  });

  it('reconcile with a STALE snapshot skips wholesale but still forces removals', async () => {
    // Live SSE already advanced local meta to rev 6 and added a row; a reconcile
    // GET that started at rev 5 is now stale and must not roll state backward.
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 6 });
    await db.list_entries.put({
      id: 'sse-added',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'sse-added',
      display_name: 'sse-added',
      quantity: 1,
      status: 'active',
      last_updated_revision: 6,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    // A rejected-add row that must STILL be force-removed despite the stale skip.
    await db.list_entries.put({
      id: 'rejected-add',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'rejected-add',
      display_name: 'rejected-add',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 0,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    // Stale rev-5 snapshot that omits 'sse-added' (it was added at rev 6).
    await applySnapshotEvent(
      LIST,
      { id: LIST, name: 'L', current_revision: 5, entries: [] } as unknown as SseSnapshotData,
      { reconcile: true, removeEntityIds: ['rejected-add'] }
    );

    // Wholesale skipped: meta not rolled back, sse-added not deleted.
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(6);
    expect(await db.list_entries.get('sse-added')).toBeDefined();
    // Forced removal still happened.
    expect(await db.list_entries.get('rejected-add')).toBeUndefined();
  });

  it('stale reconcile reverts a failed non-add entity (guarded) without rollback', async () => {
    // Local meta advanced to rev 6 by SSE on a DIFFERENT row; a failed mark_bought
    // on entity X (still at its prior rev 5) must revert to the snapshot's active X.
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 6 });
    // X: optimistically bought locally, server never changed it (still rev 5).
    await db.list_entries.put({
      id: 'x',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'x',
      display_name: 'x',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 5,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    // An unrelated active row added by SSE at rev 6, absent from the rev-5 snapshot.
    await db.list_entries.put({
      id: 'unrelated',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'unrelated',
      display_name: 'unrelated',
      quantity: 1,
      status: 'active',
      last_updated_revision: 6,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    await applySnapshotEvent(
      LIST,
      {
        id: LIST,
        name: 'L',
        current_revision: 5,
        entries: [snapshotEntity('x', 'active', 5)]
      } as unknown as SseSnapshotData,
      { reconcile: true, revertEntityIds: ['x'] }
    );

    expect((await db.list_entries.get('x'))?.status).toBe('active'); // reverted
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(6); // no rollback
    expect(await db.list_entries.get('unrelated')).toBeDefined(); // not deleted
  });

  it('stale reconcile does NOT revert a non-add entity SSE advanced to a higher rev', async () => {
    // SSE advanced entity X itself to rev 6 (bought); the stale rev-5 snapshot's
    // older active X must NOT clobber it (per-entry monotonic guard).
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 6 });
    await db.list_entries.put({
      id: 'x',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'x',
      display_name: 'x',
      quantity: 1,
      status: 'bought',
      bought_at: '2026-06-03T00:00:00.000Z',
      last_updated_revision: 6,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    await applySnapshotEvent(
      LIST,
      {
        id: LIST,
        name: 'L',
        current_revision: 5,
        entries: [snapshotEntity('x', 'active', 5)]
      } as unknown as SseSnapshotData,
      { reconcile: true, revertEntityIds: ['x'] }
    );

    expect((await db.list_entries.get('x'))?.status).toBe('bought'); // SSE state kept
  });

  it('stale reconcile reverts a failed delete_item entity to active (guarded)', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 6 });
    await db.list_entries.put({
      id: 'd',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'd',
      display_name: 'd',
      quantity: 1,
      status: 'deleted',
      last_updated_revision: 5,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    await applySnapshotEvent(
      LIST,
      {
        id: LIST,
        name: 'L',
        current_revision: 5,
        entries: [snapshotEntity('d', 'active', 5)]
      } as unknown as SseSnapshotData,
      { reconcile: true, revertEntityIds: ['d'] }
    );

    expect((await db.list_entries.get('d'))?.status).toBe('active'); // reverted
  });

  it('reconcile with a NEWER snapshot applies the wholesale reconcile', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    await db.list_entries.put({
      id: 'old-active',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'old-active',
      display_name: 'old-active',
      quantity: 1,
      status: 'active',
      last_updated_revision: 5,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    // Newer rev-7 snapshot that no longer lists 'old-active'.
    await applySnapshotEvent(
      LIST,
      {
        id: LIST,
        name: 'L',
        current_revision: 7,
        entries: [snapshotEntity('new-active', 'active', 7)]
      } as unknown as SseSnapshotData,
      { reconcile: true }
    );

    expect((await db.list_meta.get(LIST))?.current_revision).toBe(7);
    expect(await db.list_entries.get('old-active')).toBeUndefined(); // reconciled away
    expect(await db.list_entries.get('new-active')).toBeDefined();
  });

  it('applySnapshotEvent does not roll the revision (and entries) backwards', async () => {
    await db.list_entries.put({
      id: 'fresh',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'fresh',
      display_name: 'fresh',
      quantity: 1,
      status: 'active',
      last_updated_revision: 9,
      updated_at: '2026-06-03T00:00:00.000Z'
    });
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 9 });

    // A stale snapshot (revision 4 < local 9) must be skipped wholesale.
    await applySnapshotEvent(LIST, {
      id: LIST,
      name: 'Stale',
      current_revision: 4,
      entries: [snapshotEntity('old', 'active', 4)]
    } as unknown as SseSnapshotData);

    expect((await db.list_meta.get(LIST))?.current_revision).toBe(9); // unchanged
    const ids = (await db.list_entries.where('list_id').equals(LIST).toArray()).map((e) => e.id);
    expect(ids).toEqual(['fresh']); // entries not replaced by the stale snapshot
  });

  it('applySnapshotEvent throws on a malformed payload instead of wiping state', async () => {
    await db.list_entries.put({
      id: 'keep',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'keep',
      display_name: 'keep',
      quantity: 1,
      status: 'active',
      last_updated_revision: 1,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    await expect(
      applySnapshotEvent(LIST, {
        id: LIST,
        name: 'x',
        current_revision: 5
      } as unknown as SseSnapshotData)
    ).rejects.toThrow(/malformed/);
    // Local entries survive the rejected (malformed) snapshot.
    expect(await db.list_entries.get('keep')).toBeDefined();
  });

  it('applyHeartbeat updates last_synced_at without changing the revision', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 5 });
    await applyHeartbeat(LIST);
    const meta = await db.list_meta.get(LIST);
    expect(meta?.current_revision).toBe(5);
    expect(meta?.last_synced_at).toBeDefined();
  });
});

describe('backoffFor', () => {
  it('follows the 1s→2s→4s→8s→cap 30s ladder', () => {
    expect([0, 1, 2, 3, 4, 5, 99].map(backoffFor)).toEqual([
      1000, 2000, 4000, 8000, 30000, 30000, 30000
    ]);
  });
});

describe('SseClient', () => {
  beforeEach(() => {
    FakeEventSource.instances = [];
  });

  function makeClient(extra: Partial<SseClientOptions> = {}) {
    const timers: Array<{ fn: () => void; ms: number }> = [];
    const client = new SseClient(LIST, {
      eventSourceFactory: (url) => new FakeEventSource(url),
      setTimer: (fn, ms) => {
        timers.push({ fn, ms });
        return timers.length as unknown as ReturnType<typeof setTimeout>;
      },
      clearTimer: () => {},
      ...extra
    });
    return { client, timers };
  }

  it('opens the stream with ?since=<current_revision>', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 12 });
    const { client } = makeClient();
    await client.start();
    expect(FakeEventSource.instances[0].url).toBe('/api/sse/list-1?since=12');
    client.stop();
  });

  it('opens with ?since=0 when no local meta exists yet', async () => {
    const { client } = makeClient();
    await client.start();
    expect(FakeEventSource.instances[0].url).toBe('/api/sse/list-1?since=0');
    client.stop();
  });

  it('applies a mutation event received over the stream', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const { client } = makeClient();
    await client.start();
    FakeEventSource.instances[0].emit('mutation', {
      server_revision: 3,
      entity: serverEntity('e1', 'bought', 3)
    });
    // Applier runs on a microtask; let it settle.
    await vi.waitFor(async () => {
      expect((await db.list_entries.get('e1'))?.status).toBe('bought');
    });
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(3);
    client.stop();
  });

  it('reconnects with exponential backoff and re-reads the advanced revision', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 1 });
    const delays: number[] = [];
    const { client, timers } = makeClient({ onReconnectScheduled: (ms) => delays.push(ms) });
    await client.start();

    // First connection drops.
    FakeEventSource.instances[0].fail();
    expect(delays).toEqual([1000]);
    expect(FakeEventSource.instances[0].closed).toBe(true);

    // Meanwhile a mutation advanced the local revision; the reconnect must use it.
    await db.list_meta.update(LIST, { current_revision: 5 });
    timers[0].fn(); // fire the scheduled reconnect (connect() is async)
    await vi.waitFor(() => expect(FakeEventSource.instances).toHaveLength(2));
    expect(FakeEventSource.instances[1].url).toBe('/api/sse/list-1?since=5');

    // Second drop → backoff doubles.
    FakeEventSource.instances[1].fail();
    expect(delays).toEqual([1000, 2000]);
    client.stop();
  });

  it('resets the backoff ladder after a successful reconnect', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const delays: number[] = [];
    const { client, timers } = makeClient({ onReconnectScheduled: (ms) => delays.push(ms) });
    await client.start();

    FakeEventSource.instances[0].fail(); // delay 1000
    timers[0].fn();
    await vi.waitFor(() => expect(FakeEventSource.instances).toHaveLength(2));
    FakeEventSource.instances[1].open(); // clean connection resets failures
    FakeEventSource.instances[1].fail(); // back to 1000, not 2000
    expect(delays).toEqual([1000, 1000]);
    client.stop();
  });

  it('schedules only one reconnect when EventSource fires repeated errors', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const delays: number[] = [];
    const { client, timers } = makeClient({ onReconnectScheduled: (ms) => delays.push(ms) });
    await client.start();

    // EventSource commonly emits several error events for a single drop.
    FakeEventSource.instances[0].fail();
    FakeEventSource.instances[0].fail();
    FakeEventSource.instances[0].fail();

    // Only one timer armed (and thus only one onReconnectScheduled), not three.
    expect(timers).toHaveLength(1);
    expect(delays).toEqual([1000]);

    // Firing it opens exactly one new connection.
    timers[0].fn();
    await vi.waitFor(() => expect(FakeEventSource.instances).toHaveLength(2));
    client.stop();
  });

  it('an SSE mutation and a drain applying the same entry: server-authoritative wins', async () => {
    // Simulates the SSE-vs-drain race the reviewer called out: the live stream
    // delivers a newer authoritative copy of an entry that a queued mutation
    // also touched. The server is authoritative, so the higher-revision SSE
    // payload must be the one that survives in list_entries.
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 1 });
    await db.list_entries.put({
      id: 'e1',
      list_id: LIST,
      resolution_status: 'unresolved',
      raw_text: 'e1',
      display_name: 'e1',
      quantity: 1,
      status: 'active',
      last_updated_revision: 1,
      updated_at: '2026-06-03T00:00:00.000Z'
    });

    // Drain confirms the entry as `active` at revision 2.
    await applyMutationEvent({ server_revision: 2, entity: serverEntity('e1', 'active', 2) });
    // SSE then delivers the authoritative `bought` at the higher revision 3.
    await applyMutationEvent({ server_revision: 3, entity: serverEntity('e1', 'bought', 3) });

    expect((await db.list_entries.get('e1'))?.status).toBe('bought');
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(3);
  });

  it('stop() closes the source and prevents reconnects', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const { client } = makeClient();
    await client.start();
    const source = FakeEventSource.instances[0];
    client.stop();
    expect(source.closed).toBe(true);
    // A late error after stop must not schedule a reconnect.
    source.fail();
    expect(FakeEventSource.instances).toHaveLength(1);
  });

  it('stop() during the in-flight connect() leaves no live source (no leak)', async () => {
    await db.list_meta.put({ id: LIST, name: 'L', current_revision: 0 });
    const { client } = makeClient();
    // connect() suspends on the real `db.list_meta.get` await; stop() runs
    // synchronously in that window. Without the post-await recheck, connect()
    // would resume and open a live EventSource that nothing ever closes.
    const p = client.start();
    client.stop();
    await p;
    // The factory must not have left a live source: either it was never called,
    // or any source it created is closed.
    expect(FakeEventSource.instances.every((s) => s.closed)).toBe(true);
  });
});
