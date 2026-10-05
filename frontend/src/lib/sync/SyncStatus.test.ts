import { beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import { db, type LocalMutation } from '$lib/db/dexie';
import {
  createSyncStatusStore,
  deriveSyncStatus,
  syncStatusLabel
} from './SyncStatus';

const LIST = 'list-status';

beforeEach(async () => {
  await db.delete();
  await db.open();
});

function mutation(entityId: string, status: LocalMutation['status']): LocalMutation {
  return {
    client_mutation_id: `cmid-${entityId}`,
    device_id: 'dev-1',
    list_id: LIST,
    entity_type: 'list_entry',
    entity_id: entityId,
    operation_type: 'add_item',
    payload: {},
    base_revision: 0,
    created_at_client: '2026-06-03T00:00:00.000Z',
    status,
    retry_count: 0
  };
}

describe('deriveSyncStatus', () => {
  it('is synced when online, connected, and nothing is queued', () => {
    expect(deriveSyncStatus({ online: true, connected: true }).state).toBe('synced');
  });

  it('is syncing only while a drain is in flight', () => {
    const status = deriveSyncStatus({ online: true, connected: true, pending: 3, syncing: true });
    expect(status.state).toBe('syncing');
    expect(syncStatusLabel(status)).toBe('Syncing 3 changes…');
  });

  it('reports queued changes as offline when the drain has stalled', () => {
    const status = deriveSyncStatus({ online: true, connected: true, pending: 3 });
    expect(status.state).toBe('offline');
    expect(syncStatusLabel(status)).toBe('Offline — 3 changes pending');
  });

  it('is syncing while a drain is in flight even with nothing queued', () => {
    expect(deriveSyncStatus({ online: true, connected: true, syncing: true }).state).toBe('syncing');
  });

  it('is offline with the pending count when the browser is offline', () => {
    const status = deriveSyncStatus({ online: false, connected: false, pending: 2 });
    expect(status.state).toBe('offline');
    expect(syncStatusLabel(status)).toBe('Offline — 2 changes pending');
  });

  it('is offline when the server is unreachable and changes are queued', () => {
    expect(deriveSyncStatus({ online: true, connected: false, pending: 1 }).state).toBe('offline');
  });

  it('stays synced with nothing queued even if the live stream is down', () => {
    expect(deriveSyncStatus({ online: true, connected: false, pending: 0 }).state).toBe('synced');
  });

  it('reports an error when a change was terminally rejected', () => {
    const status = deriveSyncStatus({ online: true, connected: true, failed: 1 });
    expect(status.state).toBe('error');
    expect(syncStatusLabel(status)).toBe('Sync error — 1 change couldn’t be saved');
  });

  it('gives the error state precedence over offline', () => {
    expect(deriveSyncStatus({ online: false, connected: false, failed: 1 }).state).toBe('error');
  });

  it('labels offline with nothing pending plainly and defaults to synced', () => {
    expect(syncStatusLabel(deriveSyncStatus({ online: false }))).toBe('Offline');
    expect(deriveSyncStatus().state).toBe('synced');
  });

  it('clamps negative and fractional counts', () => {
    const status = deriveSyncStatus({ pending: -2, failed: 1.9 });
    expect(status.pending).toBe(0);
    expect(status.failed).toBe(1);
  });
});

describe('createSyncStatusStore', () => {
  it('follows the real queue and connection state from synced through pending to synced', async () => {
    const store = createSyncStatusStore(LIST);
    const unsubscribe = store.status.subscribe(() => {});
    try {
      expect(get(store.status).state).toBe('synced');

      // A queued edit without a drain is stalled; the in-flight flag is the
      // only state that makes it syncing.
      await db.mutation_queue.add(mutation('e1', 'pending'));
      await vi.waitFor(() =>
        expect(get(store.status)).toMatchObject({ state: 'offline', pending: 1 })
      );
      store.setSyncing(true);
      expect(get(store.status)).toMatchObject({ state: 'syncing', pending: 1 });

      // The browser losing its network → offline with the pending count.
      window.dispatchEvent(new Event('offline'));
      expect(get(store.status)).toMatchObject({ state: 'offline', pending: 1 });
      expect(syncStatusLabel(get(store.status))).toBe('Offline — 1 change pending');

      // Reconnect flips the connection flag back on optimistically.
      window.dispatchEvent(new Event('online'));
      expect(get(store.status).state).toBe('syncing');

      // The server confirming the mutation clears the count → synced again.
      const [row] = await db.mutation_queue.toArray();
      await db.mutation_queue.update(row.id!, { status: 'confirmed' });
      await vi.waitFor(() =>
        expect(get(store.status)).toMatchObject({ state: 'synced', pending: 0 })
      );
      store.setSyncing(false);

      // A terminal failure surfaces from persisted queue state and clears when
      // its row is dismissed from the queue.
      await db.mutation_queue.add(mutation('e-failed', 'failed'));
      await vi.waitFor(() => expect(get(store.status)).toMatchObject({ state: 'error', failed: 1 }));
      await db.mutation_queue.where('status').equals('failed').delete();
      await vi.waitFor(() => expect(get(store.status).state).toBe('synced'));
    } finally {
      unsubscribe();
    }
  });

  it('shows a persisted failure when a fresh status store subscribes after reload', async () => {
    await db.mutation_queue.add(mutation('e-reload-failed', 'failed'));

    const storeAfterReload = createSyncStatusStore(LIST);
    const unsubscribe = storeAfterReload.status.subscribe(() => {});
    try {
      await vi.waitFor(() => {
        expect(get(storeAfterReload.status)).toMatchObject({ state: 'error', failed: 1 });
        expect(syncStatusLabel(get(storeAfterReload.status))).toBe(
          'Sync error — 1 change couldn’t be saved'
        );
      });
    } finally {
      unsubscribe();
    }
  });

  it('treats a closed stream as offline while changes are queued', async () => {
    const store = createSyncStatusStore(LIST);
    const unsubscribe = store.status.subscribe(() => {});
    try {
      // Nothing queued: a dropped stream alone is not "unsynced".
      store.setConnected(false);
      expect(get(store.status).state).toBe('synced');

      // With an edit queued and no stream, we cannot reach the server → offline.
      await db.mutation_queue.add(mutation('e2', 'pending'));
      await vi.waitFor(() =>
        expect(get(store.status)).toMatchObject({ state: 'offline', pending: 1 })
      );
    } finally {
      unsubscribe();
    }
  });
});
