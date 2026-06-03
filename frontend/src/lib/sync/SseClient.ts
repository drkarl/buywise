// SSE client: live updates from the server.
//
// After the queue drains (see SyncQueue), we open an EventSource to
// `GET /api/sse/:list_id?since=<current_revision>`. The server backfills any
// mutations newer than `since` and then streams live events (plan P1-T5):
//   - `mutation`      → upsert the entity into `list_entries`, advance revision
//   - `full_snapshot` → replace ALL entries for this list (client was beyond the
//                       server's retention window), reset revision to snapshot
//   - `heartbeat`     → bump `last_synced_at` (liveness only)
//   - error / close   → reconnect with exponential backoff 1s→2s→4s→8s→cap 30s
//
// The server is authoritative: every entity carried by an event replaces the
// local copy (see design §11, plan P2-T4 conflict handling).

import { db } from '$lib/db/dexie';
import {
  toLocalEntry,
  type SseMutationData,
  type SseSnapshotData
} from './types';

/** Reconnect backoff ladder in milliseconds (plan P2-T4): 1s→2s→4s→8s, cap 30s. */
const BACKOFF_MS = [1000, 2000, 4000, 8000, 30000];

/** Resolves the reconnect delay for the Nth consecutive failure (0-based). */
export function backoffFor(attempt: number): number {
  return BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
}

/** Minimal EventSource surface we depend on — lets tests supply a fake. */
export interface EventSourceLike {
  addEventListener(type: string, listener: (event: MessageEvent) => void): void;
  close(): void;
  onerror: ((event: Event) => void) | null;
  onopen: ((event: Event) => void) | null;
}

export type EventSourceFactory = (url: string) => EventSourceLike;

export interface SseClientOptions {
  /** Build the EventSource (tests inject a fake). Defaults to the global. */
  eventSourceFactory?: EventSourceFactory;
  /** Schedule the reconnect timer (tests inject fake timers). */
  setTimer?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  /** Clear a scheduled reconnect timer. */
  clearTimer?: (handle: ReturnType<typeof setTimeout>) => void;
  /** Notified after a successful (re)connection resets the backoff. */
  onOpen?: () => void;
  /** Notified on each disconnect, with the upcoming reconnect delay (ms). */
  onReconnectScheduled?: (delayMs: number) => void;
}

/** Applies a `mutation` event: authoritative upsert + monotonic revision bump. */
export async function applyMutationEvent(data: SseMutationData): Promise<void> {
  const entry = toLocalEntry(data.entity);
  await db.transaction('rw', db.list_entries, db.list_meta, async () => {
    // Per-entry monotonic guard: only overwrite the local row if the incoming
    // entity is at least as new (by `last_updated_revision`). Events for the same
    // entry can arrive out of order across the SSE and drain paths (e.g. an SSE
    // delete at rev 3 racing a delayed POST response at rev 2); writing the older
    // entity would clobber the newer one while meta stays advanced, leaving a
    // stale row a reconnect won't repair. Read-modify-write stays in this txn.
    const existing = await db.list_entries.get(entry.id);
    if (!existing || entry.last_updated_revision >= existing.last_updated_revision) {
      await db.list_entries.put(entry);
    }
    const meta = await db.list_meta.get(entry.list_id);
    const now = new Date().toISOString();
    if (meta) {
      const next =
        data.server_revision > meta.current_revision
          ? data.server_revision
          : meta.current_revision;
      await db.list_meta.update(entry.list_id, {
        current_revision: next,
        last_synced_at: now
      });
    } else {
      await db.list_meta.put({
        id: entry.list_id,
        name: '',
        current_revision: data.server_revision,
        last_synced_at: now
      });
    }
  });
}

/**
 * Applies a `full_snapshot` event (also reused by the initial `GET /api/list/:id`
 * fetch). `listId` is supplied by the caller — the SSE client knows its own list
 * id, and `fetchSnapshot` knows the id it requested — because the server's
 * `EntryView` rows OMIT `list_id` (see `EntryView` in crates/api/src/lists.rs),
 * so we cannot read it off each entry. We stamp `listId` onto every snapshot row
 * here.
 *
 * IMPORTANT — the snapshot is ACTIVE-ONLY. `GET /api/list/:id` / `full_snapshot`
 * serialise `WHERE status = 'active'` only (confirmed: `load_detail` /
 * `list_snapshot` in lists.rs); they do NOT enumerate bought/skipped/deleted
 * rows. A naive wholesale "delete all rows for this list, then bulkPut" would
 * therefore destroy local basket history (bought/skipped). So the destructive
 * replace is SCOPED TO ACTIVE ENTRIES: we reconcile the local `status='active'`
 * set to exactly the snapshot's active set (upsert present, delete local actives
 * absent from the snapshot) and PRESERVE local non-active rows untouched. This is
 * a deliberate workaround for the active-only endpoint (see the backend gap noted
 * in the P2-T4 report).
 *
 * One exception to the delete: an active row backing a not-yet-confirmed mutation
 * (`mutation_queue` status `pending` or `sent`) is an OPTIMISTIC local insert the
 * server snapshot cannot know about yet (e.g. an offline `add_item` whose POST
 * hasn't drained). Deleting it would make the user's add/check visibly vanish
 * while its mutation is still queued. So such ids are preserved.
 *
 * `reconcile`: a terminal 4xx rejection does NOT bump the server revision, so
 * after a failed drain `meta.current_revision === snapshot.current_revision` and
 * the non-reconcile short-circuit would skip the whole apply, leaving the phantom
 * optimistic row in place. When `runSync` detects failures it re-fetches with
 * `{ reconcile: true }`, which allows the EQUAL-revision 4xx cleanup and any NEWER
 * snapshot, but STILL skips the wholesale apply for a STALE snapshot (strictly
 * older than local meta — e.g. live SSE advanced local state while the reconcile
 * GET was in flight) so it can't roll entries/revision backward. The forced
 * `removeEntityIds` deletions are revision-independent and run even when the
 * wholesale apply is skipped (see below).
 *
 * `removeEntityIds`: entity ids to delete UNCONDITIONALLY, regardless of local
 * status AND regardless of snapshot revision. `runSync` passes the ids of
 * terminally-rejected `add_item` mutations here. `add_item` is the only op that
 * creates a client-side entity, so a rejected add means the id is bogus in ANY
 * status — including a row the user marked bought/skipped before the add drained
 * (which the active-scoped `staleActive` delete would otherwise miss, leaving it
 * stuck in the basket). The deletion happens up front, before any stale-skip
 * decision, so it applies even when a stale wholesale reconcile is skipped. These
 * ids are `failed` (so absent from `pendingEntityIds`) and absent from the
 * snapshot (so `bulkPut` won't re-add them). Failed NON-add ops
 * (mark_bought/skipped/delete) act on entities that DO exist server-side, so they
 * are NOT removed — instead `runSync` passes them as `revertEntityIds` so the
 * authoritative `active` row is restored. Legitimate bought/skipped history
 * (entities whose add succeeded) is never touched.
 *
 * `revertEntityIds`: entity ids of failed NON-add ops (mark_bought/skipped/delete)
 * to revert to the snapshot's authoritative copy. The EQUAL/NEWER wholesale path
 * already reverts these via `bulkPut`, but the STALE path returns before that
 * upsert — so the stale branch performs a SELECTIVE, per-entry revision-guarded
 * revert for these ids only (`put` the snapshot's row iff the local row is absent
 * or `snapshotEntry.last_updated_revision >= localRow.last_updated_revision`).
 * The guard is essential: if live SSE legitimately advanced that same entity to a
 * higher revision during the in-flight reconcile GET, the stale snapshot's older
 * copy must not clobber it; a failed `mark_bought` that never changed server state
 * leaves the local row at its prior (equal) revision, so `>=` allows the revert.
 */
export interface ApplySnapshotOptions {
  /** Bypass the revision short-circuit to reconcile after a rejected drain. */
  reconcile?: boolean;
  /** Entity ids to delete regardless of status (terminally-rejected add_items). */
  removeEntityIds?: Iterable<string>;
  /** Entity ids of failed non-add ops to revert to the snapshot's copy (guarded). */
  revertEntityIds?: Iterable<string>;
}

export async function applySnapshotEvent(
  listId: string,
  snapshot: SseSnapshotData,
  options: ApplySnapshotOptions = {}
): Promise<void> {
  // Defensive parse guard: a snapshot with a missing/garbled `entries` array
  // would otherwise silently wipe local active state. The wire shape is
  // `GET /api/list/:id` / Rust `ListDetail` (plan P1-T3/T5), whose `entries` is
  // always an array. Surface a malformed event instead of no-oping.
  if (!snapshot || !Array.isArray(snapshot.entries) || typeof snapshot.id !== 'string') {
    throw new Error('[SseClient] malformed full_snapshot payload');
  }
  // Stamp the known list id onto every row (server EntryView omits list_id).
  const entries = snapshot.entries.map((e) => ({ ...toLocalEntry(e), list_id: listId }));
  const snapshotIds = new Set(entries.map((e) => e.id));
  const snapshotById = new Map(entries.map((e) => [e.id, e]));

  await db.transaction('rw', db.list_entries, db.list_meta, db.mutation_queue, async () => {
    const meta = await db.list_meta.get(listId);

    // Forced removals (rejected add_item ids) are revision-INDEPENDENT: a rejected
    // add's entity is bogus regardless of snapshot age, so delete it up front,
    // unconditionally, before any skip decision. This keeps the cleanup correct
    // even when we skip a stale wholesale reconcile below.
    const forced = new Set(options.removeEntityIds ?? []);
    if (forced.size > 0) await db.list_entries.bulkDelete([...forced]);

    // The WHOLESALE active-set reconcile (staleActive delete + bulkPut + meta.put)
    // is revision-DEPENDENT and must not roll local state backward:
    //   - non-reconcile: skip if local is already >= the snapshot (a stale GET
    //     racing applied SSE mutations would otherwise undo them).
    //   - reconcile: the 4xx cleanup runs at the SAME revision (allowed), and a
    //     NEWER snapshot is allowed, but a STALE snapshot (strictly older than
    //     local meta — e.g. live SSE advanced to rev 6 while the reconcile GET at
    //     rev 5 was in flight) must skip the wholesale apply so it can't delete
    //     rows the newer state added or downgrade current_revision. The forced
    //     removals above still applied either way.
    const skipWholesale = options.reconcile
      ? !!meta && snapshot.current_revision < meta.current_revision
      : !!meta && meta.current_revision >= snapshot.current_revision;
    if (skipWholesale) {
      // Stale reconcile: we skipped the wholesale upsert, so failed non-add ops
      // (mark_bought/skipped/delete) would otherwise stay stuck in their optimistic
      // status. Selectively revert those ids to the snapshot's authoritative copy,
      // per-entry monotonic-guarded so a concurrently-advanced (higher-revision)
      // SSE row is never clobbered by the stale snapshot's older copy.
      for (const id of options.revertEntityIds ?? []) {
        const snapEntry = snapshotById.get(id);
        if (!snapEntry) continue; // not in this snapshot — nothing authoritative to revert to
        const local = await db.list_entries.get(id);
        if (!local || snapEntry.last_updated_revision >= local.last_updated_revision) {
          await db.list_entries.put(snapEntry);
        }
      }
      return;
    }

    // Ids of entries backing a queued/in-flight (not-yet-confirmed) mutation.
    // Read inside this transaction so a concurrent drain can't race the delete.
    const pendingEntityIds = new Set(
      (
        await db.mutation_queue
          .where('list_id')
          .equals(listId)
          .filter((m) => m.status === 'pending' || m.status === 'sent')
          .toArray()
      ).map((m) => m.entity_id)
    );

    // Reconcile ONLY the active set: delete local active rows the snapshot no
    // longer lists (e.g. another device bought/deleted them), then upsert the
    // snapshot's actives. Non-active rows (bought/skipped/deleted) are preserved
    // because the active-only endpoint can't enumerate them. Optimistic rows
    // backing a pending/in-flight mutation are also preserved (see doc above).
    const localActiveIds = await db.list_entries
      .where('list_id')
      .equals(listId)
      .filter((e) => e.status === 'active')
      .primaryKeys();
    const staleActive = localActiveIds.filter(
      (id) => !snapshotIds.has(id) && !pendingEntityIds.has(id)
    );
    if (staleActive.length > 0) await db.list_entries.bulkDelete(staleActive);
    if (entries.length > 0) await db.list_entries.bulkPut(entries);

    const now = new Date().toISOString();
    await db.list_meta.put({
      id: listId,
      name: snapshot.name || meta?.name || '',
      current_revision: snapshot.current_revision,
      last_synced_at: now
    });
  });
}

/**
 * Bumps `last_synced_at` on a heartbeat (liveness only — no entries change).
 *
 * NOTE: `last_synced_at` is overloaded — both confirmed mutations / snapshots
 * AND bare heartbeats stamp it, so it means "last time we heard from the server"
 * (last-seen), not strictly "last time data changed". A dedicated `last_seen_at`
 * field would disambiguate but requires a schema migration (the P1-T2 Dexie
 * schema only defines `last_synced_at`); deferred as out of scope for P2-T4.
 */
export async function applyHeartbeat(listId: string): Promise<void> {
  const now = new Date().toISOString();
  const meta = await db.list_meta.get(listId);
  if (meta) {
    await db.list_meta.update(listId, { last_synced_at: now });
  }
}

/**
 * Long-lived SSE subscription for one list with automatic reconnect.
 *
 * `start()` opens the stream at the list's current revision; on any error/close
 * it reconnects with exponential backoff, re-reading the (possibly advanced)
 * revision each time so reconnects resume from the right `?since=`. `stop()`
 * closes the stream and cancels any pending reconnect.
 */
export class SseClient {
  private readonly listId: string;
  private readonly factory: EventSourceFactory;
  private readonly setTimer: NonNullable<SseClientOptions['setTimer']>;
  private readonly clearTimer: NonNullable<SseClientOptions['clearTimer']>;
  private readonly onOpen?: () => void;
  private readonly onReconnectScheduled?: (delayMs: number) => void;

  private source: EventSourceLike | null = null;
  private reconnectHandle: ReturnType<typeof setTimeout> | null = null;
  private failures = 0;
  private stopped = false;

  constructor(listId: string, options: SseClientOptions = {}) {
    this.listId = listId;
    this.factory =
      options.eventSourceFactory ??
      ((url: string) => new EventSource(url) as unknown as EventSourceLike);
    this.setTimer = options.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = options.clearTimer ?? ((h) => clearTimeout(h));
    this.onOpen = options.onOpen;
    this.onReconnectScheduled = options.onReconnectScheduled;
  }

  /** Opens (or re-opens) the stream at the list's current revision. */
  async start(): Promise<void> {
    this.stopped = false;
    await this.connect();
  }

  /** Closes the stream and cancels any scheduled reconnect. Idempotent. */
  stop(): void {
    this.stopped = true;
    if (this.reconnectHandle !== null) {
      this.clearTimer(this.reconnectHandle);
      this.reconnectHandle = null;
    }
    if (this.source) {
      this.source.close();
      this.source = null;
    }
  }

  private async connect(): Promise<void> {
    if (this.stopped) return;

    const meta = await db.list_meta.get(this.listId);
    // Recheck after the await: stop() may have run during the `list_meta.get`,
    // setting `stopped = true` while `this.source` was still null (nothing to
    // close). The create-and-assign below is synchronous with no further await,
    // so this recheck fully closes the race window — without it we'd open a live
    // SSE connection for an unmounted view that nothing would ever close (leak).
    if (this.stopped) return;

    const since = meta?.current_revision ?? 0;
    const url = `/api/sse/${encodeURIComponent(this.listId)}?since=${since}`;

    const source = this.factory(url);
    this.source = source;

    source.onopen = () => {
      // A clean (re)connection clears the backoff ladder.
      this.failures = 0;
      this.onOpen?.();
    };

    source.addEventListener('mutation', (event) => {
      void this.handle(() => applyMutationEvent(JSON.parse(event.data) as SseMutationData));
    });
    source.addEventListener('full_snapshot', (event) => {
      void this.handle(() =>
        applySnapshotEvent(this.listId, JSON.parse(event.data) as SseSnapshotData)
      );
    });
    source.addEventListener('heartbeat', () => {
      void this.handle(() => applyHeartbeat(this.listId));
    });

    source.onerror = () => {
      this.scheduleReconnect();
    };
  }

  /** Applies an event, swallowing parse/Dexie errors so the stream survives. */
  private async handle(apply: () => Promise<void>): Promise<void> {
    try {
      await apply();
    } catch (err) {
      console.error('[SseClient] failed to apply event', err);
    }
  }

  private scheduleReconnect(): void {
    if (this.stopped) return;
    // EventSource often fires repeated `error` events for a single drop, and
    // each one calls scheduleReconnect(). If a reconnect timer is already armed,
    // do nothing — otherwise we'd stack timers and open several concurrent
    // connections when they all fire.
    if (this.reconnectHandle !== null) return;
    // Close the dead source before reconnecting so we don't leak listeners.
    if (this.source) {
      this.source.close();
      this.source = null;
    }
    const delay = backoffFor(this.failures);
    this.failures += 1;
    this.onReconnectScheduled?.(delay);
    this.reconnectHandle = this.setTimer(() => {
      this.reconnectHandle = null;
      void this.connect();
    }, delay);
  }
}
