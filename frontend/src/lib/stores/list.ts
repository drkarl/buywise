// Single source of truth for list state and mutations.
//
// Every user action does two things, atomically and offline-first:
//   1. enqueues a `LocalMutation` (status: 'pending') in `mutation_queue`, and
//   2. optimistically updates the `LocalListEntry` in `list_entries`.
// The UI renders straight off Dexie via `liveQuery`, so it never waits for the
// server. The sync loop (P2-T4) drains the queue when online; NO network code
// lives here. After each enqueue we fire `notifyMutationEnqueued(listId)` — a
// decoupled in-process signal (not network) — so a running sync engine flushes
// the new mutation immediately instead of leaving it `pending` until a refresh.

import { liveQuery, type Observable } from 'dexie';
import { readable, type Readable } from 'svelte/store';
import {
  db,
  type LocalListEntry,
  type LocalMutation
} from '$lib/db/dexie';
import { getDeviceId } from '$lib/db/device';
import { notifyMutationEnqueued } from '$lib/sync/enqueueSignal';

/**
 * Upper bound for `set_quantity` (issue #60). Mirrors the backend's `MAX_QUANTITY`
 * (`crates/api/src/mutations.rs`, `1..=9999`); the server rejects anything above it
 * with 422. The single frontend source of truth — `ListItem.svelte` imports this to
 * disable the `+` button at the ceiling. Keep in sync with the backend constant.
 */
export const MAX_QUANTITY = 9999;

/** Operation types in scope for P2-T3 + P3-T3 (subset of the canonical enum). */
type OperationType =
  | 'add_item'
  | 'mark_bought'
  | 'mark_skipped'
  | 'delete_item'
  | 'resolve_alias'
  | 'set_quantity';

/**
 * Bridges a Dexie `liveQuery` Observable to a Svelte store. The query re-runs
 * automatically whenever any table it touches changes, pushing fresh values to
 * subscribers — this is what makes the header count and sections reactive.
 */
function liveStore<T>(querier: () => T | Promise<T>, initial: T): Readable<T> {
  const observable: Observable<T> = liveQuery(querier);
  return readable<T>(initial, (set) => {
    const subscription = observable.subscribe({
      next: (value) => set(value),
      error: (err) => console.error('[list store] liveQuery error', err)
    });
    return () => subscription.unsubscribe();
  });
}

/** Active (not yet bought/skipped/deleted) entries for a list, oldest first. */
export function activeEntries(listId: string): Readable<LocalListEntry[]> {
  return liveStore<LocalListEntry[]>(
    () =>
      db.list_entries
        .where('list_id')
        .equals(listId)
        .filter((e) => e.status === 'active')
        .toArray(),
    []
  );
}

/**
 * Bought + skipped entries ("In basket"). Deleted entries are never surfaced.
 */
export function basketEntries(listId: string): Readable<LocalListEntry[]> {
  return liveStore<LocalListEntry[]>(
    () =>
      db.list_entries
        .where('list_id')
        .equals(listId)
        .filter((e) => e.status === 'bought' || e.status === 'skipped')
        .toArray(),
    []
  );
}

/** Resolves the current server revision for a list (0 if unknown). */
async function currentRevision(listId: string): Promise<number> {
  const meta = await db.list_meta.get(listId);
  return meta?.current_revision ?? 0;
}

/** Builds a pending mutation skeleton shared by every operation. */
async function buildMutation(
  listId: string,
  entityId: string,
  operationType: OperationType,
  payload: object
): Promise<LocalMutation> {
  return {
    client_mutation_id: crypto.randomUUID(),
    device_id: getDeviceId(),
    list_id: listId,
    entity_type: 'list_entry',
    entity_id: entityId,
    operation_type: operationType,
    payload,
    base_revision: await currentRevision(listId),
    created_at_client: new Date().toISOString(),
    status: 'pending',
    retry_count: 0
  };
}

/**
 * Adds an item from raw text. `quantity` defaults to 1. Optimistically inserts
 * an `unresolved` active entry and enqueues an `add_item` mutation, both inside
 * one Dexie transaction so the UI and queue never diverge.
 */
export async function addItem(
  listId: string,
  rawText: string,
  quantity = 1
): Promise<string> {
  const text = rawText.trim();
  if (!text) throw new Error('addItem: rawText must not be empty');
  if (!listId) throw new Error('addItem: listId must not be empty');

  // Clamp to a sane positive integer; the UI never sends < 1 but a programmatic
  // caller could, and a non-positive quantity is meaningless for an add.
  const safeQuantity = Math.max(1, Math.floor(quantity));

  const entryId = crypto.randomUUID();
  const now = new Date().toISOString();

  const entry: LocalListEntry = {
    id: entryId,
    list_id: listId,
    canonical_item_id: undefined,
    resolution_status: 'unresolved',
    raw_text: text,
    display_name: text,
    quantity: safeQuantity,
    status: 'active',
    last_updated_revision: 0,
    updated_at: now
  };

  const mutation = await buildMutation(listId, entryId, 'add_item', {
    raw_text: text,
    display_name: text,
    quantity: safeQuantity
  });

  await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    await db.list_entries.add(entry);
    await db.mutation_queue.add(mutation);
  });

  // Committed — let a running sync engine flush it now.
  notifyMutationEnqueued(listId);
  return entryId;
}

/** Marks an entry as bought (moves it into the basket section). */
export async function markBought(listId: string, entryId: string): Promise<void> {
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'mark_bought', {
    bought_at: now
  });

  const enqueued = await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    // Load the entry first and bail unless it exists, belongs to `listId`, and is
    // not already deleted. The list-id check keeps the optimistic update and the
    // queued mutation about the SAME entry on the SAME list (and subsumes the old
    // "missing id" drift guard). The `deleted` check mirrors the backend's
    // "delete wins" rule — `apply_mark_bought` never resurrects a deleted entry —
    // so without it a stale/racing check-off after a delete would locally
    // resurrect a soft-deleted row as bought, diverging from the server. Any
    // other status (active/bought/skipped) still transitions, matching backend LWW.
    const entry = await db.list_entries.get(entryId);
    if (!entry || entry.list_id !== listId || entry.status === 'deleted') return false;
    await db.list_entries.update(entryId, {
      status: 'bought',
      bought_at: now,
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
    return true;
  });
  if (enqueued) notifyMutationEnqueued(listId);
}

/** Marks an entry as skipped (moves it into the basket section). */
export async function markSkipped(listId: string, entryId: string): Promise<void> {
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'mark_skipped', {});

  const enqueued = await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    // Validate against `listId` and skip already-deleted entries (see
    // `markBought`): the backend's "delete wins" rule never resurrects a deleted
    // entry to 'skipped', so neither does the offline UI. Other statuses transition.
    const entry = await db.list_entries.get(entryId);
    if (!entry || entry.list_id !== listId || entry.status === 'deleted') return false;
    await db.list_entries.update(entryId, {
      status: 'skipped',
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
    return true;
  });
  if (enqueued) notifyMutationEnqueued(listId);
}

/**
 * Sets an entry's quantity to an ABSOLUTE value (issue #60), the syncable form
 * of the +/- controls. Optimistically updates the local entry and enqueues a
 * `set_quantity` mutation, both in one `listId`-guarded Dexie transaction.
 *
 * The value is clamped to an integer in `1..=MAX_QUANTITY` (quantity can never go
 * below 1 nor above the server's ceiling); the `-`/`+` buttons are also disabled at
 * those bounds in the UI. An absolute value (not a delta) keeps retries/sync
 * idempotent — replaying it can never double-count.
 *
 * No-op (queues nothing) unless the entry exists, is on `listId`, is not
 * `deleted` (the backend's delete-wins rule never resurrects a deleted entry),
 * and the quantity actually changes — so tapping `+`/`-` repeatedly or a stale
 * row never spams the server with redundant mutations.
 */
export async function setQuantity(
  listId: string,
  entryId: string,
  quantity: number
): Promise<void> {
  // Clamp to an integer in `1..=MAX_QUANTITY`. The UI keeps within those bounds
  // (the `-`/`+` buttons disable at 1 and MAX_QUANTITY), but a programmatic caller
  // could exceed them: a value < 1 is meaningless, and a value > MAX_QUANTITY would
  // optimistically persist then fail server validation (422) and roll back later.
  // A non-finite input (NaN/±Infinity) falls back to 1 rather than poisoning the
  // entry — Math.floor(NaN) is NaN, which would otherwise defeat the unchanged-value
  // no-op check and persist NaN to Dexie.
  const floored = Math.floor(quantity);
  const safeQuantity = Number.isFinite(floored)
    ? Math.min(MAX_QUANTITY, Math.max(1, floored))
    : 1;
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'set_quantity', {
    quantity: safeQuantity
  });

  const enqueued = await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    // Load + re-check inside the tx (mirrors markBought/markSkipped): bail unless
    // the entry exists, belongs to `listId`, and is not deleted. Also bail when the
    // quantity is unchanged so an idempotent re-tap enqueues nothing.
    const entry = await db.list_entries.get(entryId);
    if (
      !entry ||
      entry.list_id !== listId ||
      entry.status === 'deleted' ||
      entry.quantity === safeQuantity
    ) {
      return false;
    }
    await db.list_entries.update(entryId, {
      quantity: safeQuantity,
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
    return true;
  });
  if (enqueued) notifyMutationEnqueued(listId);
}

/**
 * Confirms an `ambiguous` entry against the candidate the user chose (P3-T3).
 * Optimistically binds `canonical_item_id`, flips the entry to `resolved`, and
 * clears its candidates, then enqueues a `resolve_alias` mutation — both in one
 * Dexie transaction. The server persists the confirmed `raw_text` → canonical
 * mapping as an `accepted_match` alias.
 *
 * No-op (returns without enqueuing) unless the entry exists, is on `listId`, and
 * is still `ambiguous` — so a stale or double confirm never spams the server,
 * matching the backend's guarded bind.
 */
export async function resolveAlias(
  listId: string,
  entryId: string,
  chosenCanonicalItemId: string
): Promise<void> {
  if (!chosenCanonicalItemId) throw new Error('resolveAlias: chosen id must not be empty');
  const now = new Date().toISOString();

  // Read + build the mutation BEFORE the write transaction: `buildMutation`
  // touches `list_meta` (for base_revision), which is not in the tx table set
  // below, and Dexie forbids touching tables outside the declared scope. The
  // optimistic update then re-reads the entry inside the tx and re-checks the
  // `ambiguous` guard, so a concurrent change between read and write still no-ops.
  const existing = await db.list_entries.get(entryId);
  if (
    !existing ||
    existing.list_id !== listId ||
    existing.resolution_status !== 'ambiguous' ||
    existing.status !== 'active'
  ) {
    return;
  }
  // `raw_text` is the term the alias is learned for; the server normalises it.
  const mutation = await buildMutation(listId, entryId, 'resolve_alias', {
    chosen_canonical_item_id: chosenCanonicalItemId,
    raw_text: existing.raw_text
  });

  const enqueued = await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    const entry = await db.list_entries.get(entryId);
    // Re-check the guard inside the tx, INCLUDING `raw_text` and `status`: the
    // mutation payload captured `existing.raw_text` before the tx, so if the entry
    // was renamed between the pre-read and here, the queued `resolve_alias` would
    // carry a stale term. Likewise, if an SSE update marked the entry
    // bought/skipped/deleted while the modal sat open, it is no longer `active` and
    // confirming would optimistically resurrect it and queue a mutation the backend
    // can only no-op. Bail in either case (the server applies the same
    // ambiguous+active+raw_text-guarded bind).
    if (
      !entry ||
      entry.list_id !== listId ||
      entry.resolution_status !== 'ambiguous' ||
      entry.status !== 'active' ||
      entry.raw_text !== existing.raw_text
    ) {
      return false;
    }
    await db.list_entries.update(entryId, {
      resolution_status: 'resolved',
      canonical_item_id: chosenCanonicalItemId,
      resolution_candidates_json: undefined,
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
    return true;
  });
  if (enqueued) notifyMutationEnqueued(listId);
}

/** Soft-deletes an entry (status: 'deleted'); never shown in the UI again. */
export async function deleteItem(listId: string, entryId: string): Promise<void> {
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'delete_item', {});

  const enqueued = await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    // Validate against `listId`; `deleted` is terminal, so deleting an
    // already-deleted entry is an idempotent no-op (active/bought/skipped rows are
    // all deletable). This also stops a racing double-tap or swipe from enqueuing
    // a redundant second delete_item.
    const entry = await db.list_entries.get(entryId);
    if (!entry || entry.list_id !== listId || entry.status === 'deleted') return false;
    await db.list_entries.update(entryId, {
      status: 'deleted',
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
    return true;
  });
  if (enqueued) notifyMutationEnqueued(listId);
}
