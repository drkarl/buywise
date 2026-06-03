import { beforeEach, describe, expect, it, vi } from 'vitest';
import { db, type LocalListEntry } from '$lib/db/dexie';
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

  it('applySnapshotEvent replaces all entries and resets the revision', async () => {
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

    const snapshot: SseSnapshotData = {
      id: LIST,
      name: 'Fresh name',
      current_revision: 42,
      entries: [serverEntity('a', 'active', 40), serverEntity('b', 'bought', 41)]
    };
    await applySnapshotEvent(snapshot);

    const ids = (await db.list_entries.where('list_id').equals(LIST).toArray())
      .map((e) => e.id)
      .sort();
    expect(ids).toEqual(['a', 'b']); // 'old' gone, replaced wholesale
    expect(await db.list_entries.get('other')).toBeDefined(); // other list untouched
    expect((await db.list_meta.get(LIST))?.current_revision).toBe(42);
    expect((await db.list_meta.get(LIST))?.name).toBe('Fresh name');
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
});
