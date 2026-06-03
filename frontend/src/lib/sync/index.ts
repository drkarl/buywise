// Sync engine entry point.
//
// Wires the two halves of P2-T4 together for a single list:
//   1. drain the pending mutation queue (SyncQueue) when online, then
//   2. open the live SSE stream at the resulting revision (SseClient).
//
// `startSync(listId)` is the start-on-mount handle: call it from the list view
// `onMount` and call the returned `stop()` on destroy. It also drives a drain on
// the browser's `online` event so queued offline edits flush on reconnect.

import { db } from '$lib/db/dexie';
import { drainQueue, type FailedMutation, type SyncQueueOptions } from './SyncQueue';
import {
  SseClient,
  applySnapshotEvent,
  type SseClientOptions,
  type ApplySnapshotOptions
} from './SseClient';
import { onMutationEnqueued } from './enqueueSignal';
import type { ListSnapshot } from './types';

export { drainQueue, MAX_RETRIES } from './SyncQueue';
export type { DrainResult, FailedMutation, SyncQueueOptions } from './SyncQueue';
export {
  SseClient,
  applyMutationEvent,
  applySnapshotEvent,
  applyHeartbeat,
  backoffFor
} from './SseClient';
export type { SseClientOptions } from './SseClient';
export { notifyMutationEnqueued, onMutationEnqueued } from './enqueueSignal';
export * from './types';

export interface SyncOptions {
  fetchFn?: typeof fetch;
  sleep?: SyncQueueOptions['sleep'];
  /** Surface a server-rejected mutation to the UI (e.g. a toast). */
  onFailed?: (failure: FailedMutation) => void;
  /** EventSource injection / reconnect callbacks for the SSE half (tests). */
  sse?: SseClientOptions;
}

export interface SyncHandle {
  /** Re-runs the drain (used for pull-to-refresh and the `online` event). */
  resync(): Promise<void>;
  /** Tears down the SSE stream and detaches the `online` listener. */
  stop(): void;
}

/**
 * Fetches the authoritative `GET /api/list/:id` snapshot and replaces local
 * `list_entries` + `list_meta` for the list. Used to seed state on first sync
 * (so reconnects have a `?since=` baseline) and reused by the `full_snapshot`
 * applier. `reconcile` forwards to `applySnapshotEvent` to bypass the revision
 * short-circuit after a rejected drain. Returns the snapshot, or `null` if the
 * fetch fails (offline).
 */
export async function fetchSnapshot(
  listId: string,
  fetchFn: typeof fetch = fetch,
  options: ApplySnapshotOptions = {}
): Promise<ListSnapshot | null> {
  let response: Response;
  try {
    response = await fetchFn(`/api/list/${encodeURIComponent(listId)}`);
  } catch {
    return null; // offline — keep whatever we have locally
  }
  if (!response.ok) return null;
  const snapshot = (await response.json()) as ListSnapshot;
  await applySnapshotEvent(listId, snapshot, options);
  return snapshot;
}

/**
 * Drains the queue then ensures `list_meta` exists for the list. If we have no
 * baseline revision yet (first run for this device), pull a snapshot first so
 * the queue's `base_revision`s and the SSE `?since=` are anchored. If the drain
 * terminally rejected any mutation (4xx), re-fetch the snapshot with
 * `{ reconcile: true }` afterwards so the optimistic local state for the rejected
 * entries is corrected to server truth (the rejection doesn't bump the revision,
 * so the snapshot's normal revision short-circuit would otherwise skip the fix).
 */
async function runSync(listId: string, options: SyncOptions): Promise<void> {
  const fetchFn = options.fetchFn ?? fetch;

  const meta = await db.list_meta.get(listId);
  if (!meta) {
    // No local baseline: fetch the snapshot before draining so the revision is
    // known. (Offline → fetchSnapshot returns null and we drain against 0.)
    await fetchSnapshot(listId, fetchFn);
  }

  const result = await drainQueue(listId, {
    fetchFn,
    sleep: options.sleep,
    onFailed: options.onFailed
  });

  if (result.failed.length > 0) {
    // A 4xx means we're online, so this GET should succeed. (Offline drains leave
    // rows `pending`, not `failed`, so this never fires spuriously.)
    await fetchSnapshot(listId, fetchFn, { reconcile: true });
  }
}

/**
 * Starts the sync engine for `listId`: drains the queue, opens the SSE stream,
 * and re-drains whenever the browser reports it is back online. Safe to call in
 * `onMount`; call the returned `stop()` in the component teardown.
 */
export function startSync(listId: string, options: SyncOptions = {}): SyncHandle {
  const sse = new SseClient(listId, options.sse);

  // Single cancellation flag checked by every async continuation. stop() flips
  // it so post-teardown work (the deferred sse.start(), an in-flight resync, a
  // late `online` re-drain) becomes a no-op and never re-opens a dead stream.
  let stopped = false;

  // Single in-flight-sync guard: every drain path (bootstrap, resync, online)
  // chains onto this promise so only one runSync() runs at a time. Without it,
  // an `online` event during the initial drain would start a second concurrent
  // drainQueue() over the same pending rows (redundant POSTs / racing writes).
  let syncChain: Promise<void> = Promise.resolve();
  function enqueueSync(): Promise<void> {
    syncChain = syncChain
      .catch(() => {}) // a prior failure must not poison later runs
      .then(() => {
        if (stopped) return; // cancelled during teardown — skip
        return runSync(listId, options);
      });
    return syncChain;
  }

  // Kick off the initial drain, then open the live stream. Errors are logged,
  // not thrown, so a failed initial drain (offline) never blocks the stream or
  // the mount. The deferred sse.start() is gated on `stopped` so a stop() during
  // the initial drain does not re-open SSE after teardown.
  const bootstrap = enqueueSync()
    .catch((err) => console.error('[sync] initial drain failed', err))
    .then(() => {
      if (stopped) return; // torn down before the drain finished
      return sse.start();
    })
    .catch((err) => console.error('[sync] SSE start failed', err));

  const onOnline = () => {
    if (stopped) return;
    void enqueueSync().catch((err) => console.error('[sync] online re-drain failed', err));
  };
  if (typeof window !== 'undefined') {
    window.addEventListener('online', onOnline);
  }

  // Flush immediately when the store enqueues a mutation for THIS list while
  // online, so edits don't sit `pending` until the next refresh/`online` event.
  // Routed through the same `enqueueSync()` chain so it never races the
  // bootstrap/online drains, and gated by `stopped` so a torn-down engine is a
  // no-op (the unsubscribe in stop() also detaches it).
  const unsubscribeEnqueue = onMutationEnqueued((enqueuedListId) => {
    if (stopped || enqueuedListId !== listId) return;
    void enqueueSync().catch((err) => console.error('[sync] enqueue re-drain failed', err));
  });

  return {
    async resync() {
      await bootstrap; // ensure the initial pass has wired up before re-draining
      await enqueueSync();
    },
    stop() {
      stopped = true;
      unsubscribeEnqueue();
      if (typeof window !== 'undefined') {
        window.removeEventListener('online', onOnline);
      }
      sse.stop();
    }
  };
}
