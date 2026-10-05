// Sync / connectivity status for the always-visible indicator (issue #113).
//
// The indicator is driven entirely by REAL state — never a timer:
//   - the local mutation queue (how many edits the server has NOT confirmed),
//   - terminal 4xx failures surfaced by the drain (a change the server refused),
//   - the browser's online/offline signal, and
//   - whether the live SSE stream is currently connected.
//
// `deriveSyncStatus` is the pure state machine (unit-testable in isolation);
// `createSyncStatusStore` binds it to those live sources and exposes a Svelte
// readable the UI renders. `syncStatusLabel` turns a status into the plain text
// the indicator shows, so the state is conveyed as text and not by colour alone.

import { liveQuery } from 'dexie';
import { readable, type Readable } from 'svelte/store';
import { db } from '$lib/db/dexie';

/** The four states the indicator can show. */
export type SyncState = 'synced' | 'syncing' | 'offline' | 'error';

export interface SyncStatusInput {
  /** `navigator.onLine` — the browser believes it has a network. */
  online?: boolean;
  /** The live SSE stream is open. */
  connected?: boolean;
  /** Mutations for this list the server has not confirmed yet. */
  pending?: number;
  /** Mutations the server terminally rejected (4xx) and not yet acknowledged. */
  failed?: number;
  /** A queue drain is in flight right now. */
  syncing?: boolean;
}

export interface SyncStatus {
  state: SyncState;
  online: boolean;
  connected: boolean;
  pending: number;
  failed: number;
  syncing: boolean;
}

/**
 * Resolves the indicator state from the live inputs. Precedence:
 *
 *  1. `error`  — a change was terminally rejected; the user must know.
 *  2. `offline`— the browser is offline, OR we cannot reach the server while
 *                changes are queued (captive portal / server down: SSE closed).
 *                With nothing queued a dropped stream is not "unsynced" — there
 *                is nothing to push — so that case stays `synced`.
 *  3. `syncing`— a drain is in flight or confirmed-pending changes remain.
 *  4. `synced` — online, connected, nothing outstanding.
 */
export function deriveSyncStatus(input: SyncStatusInput = {}): SyncStatus {
  const online = input.online ?? true;
  const connected = input.connected ?? online;
  const pending = Math.max(0, Math.trunc(input.pending ?? 0));
  const failed = Math.max(0, Math.trunc(input.failed ?? 0));
  const syncing = input.syncing ?? false;

  const offline = !online || (!connected && pending > 0);
  const state: SyncState =
    failed > 0 ? 'error' : offline ? 'offline' : syncing || pending > 0 ? 'syncing' : 'synced';

  return { state, online, connected, pending, failed, syncing };
}

/** `1 change` / `N changes`. */
function countLabel(n: number): string {
  return `${n} change${n === 1 ? '' : 's'}`;
}

/**
 * Plain-text label for a status. Doubles as the indicator's accessible text so
 * the state is never conveyed by colour alone.
 */
export function syncStatusLabel(status: SyncStatus): string {
  switch (status.state) {
    case 'synced':
      return 'Synced';
    case 'syncing':
      return status.pending > 0 ? `Syncing ${countLabel(status.pending)}…` : 'Syncing…';
    case 'offline':
      return status.pending > 0 ? `Offline — ${countLabel(status.pending)} pending` : 'Offline';
    case 'error':
      return `Sync error — ${countLabel(status.failed)} couldn’t be saved`;
  }
}

export interface SyncStatusStore {
  /** The live status readable. */
  status: Readable<SyncStatus>;
  /** The SSE stream opened or dropped. */
  setConnected(connected: boolean): void;
  /** A queue drain started or finished. */
  setSyncing(syncing: boolean): void;
  /** The number of unacknowledged terminally-rejected mutations. */
  setFailed(count: number): void;
}

/**
 * Builds a status store for `listId`. The Dexie/`window` subscriptions start on
 * first subscribe and are torn down when the last subscriber leaves, so an
 * engine that is never observed has no side effects (and tests that don't
 * subscribe never touch liveQuery).
 */
export function createSyncStatusStore(listId: string): SyncStatusStore {
  let online = typeof navigator === 'undefined' || navigator.onLine !== false;
  // Optimistic at construction: the stream has not errored yet, so we assume we
  // can reach the server until SSE tells us otherwise (or the browser goes
  // offline). Without this the indicator would flash "Offline" on every mount.
  let connected = online;
  let syncing = false;
  let pending = 0;
  let failed = 0;

  let publish: (() => void) | null = null;

  const status = readable<SyncStatus>(deriveSyncStatus({ online, connected }), (set) => {
    const emit = () => set(deriveSyncStatus({ online, connected, pending, failed, syncing }));
    publish = emit;
    emit(); // reflect the current snapshot as soon as someone subscribes

    // The pending count comes straight from the local queue: it rises the
    // instant an edit is enqueued and falls when the drain confirms it.
    const subscription = liveQuery(() =>
      db.mutation_queue
        .where('list_id')
        .equals(listId)
        .filter((m) => m.status === 'pending' || m.status === 'sent')
        .count()
    ).subscribe({
      next: (count) => {
        pending = count;
        emit();
      },
      error: (err) => console.error('[sync status] queue liveQuery error', err)
    });

    const onOnline = () => {
      online = true;
      // Assume reachable again on reconnect; SSE will re-correct if it can't.
      connected = true;
      emit();
    };
    const onOffline = () => {
      online = false;
      connected = false;
      emit();
    };
    if (typeof window !== 'undefined') {
      window.addEventListener('online', onOnline);
      window.addEventListener('offline', onOffline);
    }

    return () => {
      publish = null;
      subscription.unsubscribe();
      if (typeof window !== 'undefined') {
        window.removeEventListener('online', onOnline);
        window.removeEventListener('offline', onOffline);
      }
    };
  });

  return {
    status,
    setConnected(value) {
      connected = value;
      publish?.();
    },
    setSyncing(value) {
      syncing = value;
      publish?.();
    },
    setFailed(value) {
      failed = Math.max(0, Math.trunc(value));
      publish?.();
    }
  };
}
