// Single source of truth for list state and mutations.
//
// Every user action does two things, atomically and offline-first:
//   1. enqueues a `LocalMutation` (status: 'pending') in `mutation_queue`, and
//   2. optimistically updates the `LocalListEntry` in `list_entries`.
// The UI renders straight off Dexie via `liveQuery`, so it never waits for the
// server. The sync loop (P2-T4) drains the queue when online; NO network code
// lives here.

import { liveQuery, type Observable } from 'dexie';
import { readable, type Readable } from 'svelte/store';
import {
  db,
  type LocalListEntry,
  type LocalMutation
} from '$lib/db/dexie';
import { getDeviceId } from '$lib/db/device';

/** Operation types in scope for P2-T3 (subset of the canonical enum). */
type OperationType = 'add_item' | 'mark_bought' | 'mark_skipped' | 'delete_item';

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

  const entryId = crypto.randomUUID();
  const now = new Date().toISOString();

  const entry: LocalListEntry = {
    id: entryId,
    list_id: listId,
    canonical_item_id: undefined,
    resolution_status: 'unresolved',
    raw_text: text,
    display_name: text,
    quantity,
    status: 'active',
    last_updated_revision: 0,
    updated_at: now
  };

  const mutation = await buildMutation(listId, entryId, 'add_item', {
    raw_text: text,
    display_name: text,
    quantity
  });

  await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    await db.list_entries.add(entry);
    await db.mutation_queue.add(mutation);
  });

  return entryId;
}

/** Marks an entry as bought (moves it into the basket section). */
export async function markBought(listId: string, entryId: string): Promise<void> {
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'mark_bought', {
    bought_at: now
  });

  await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    await db.list_entries.update(entryId, {
      status: 'bought',
      bought_at: now,
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
  });
}

/** Marks an entry as skipped (moves it into the basket section). */
export async function markSkipped(listId: string, entryId: string): Promise<void> {
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'mark_skipped', {});

  await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    await db.list_entries.update(entryId, {
      status: 'skipped',
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
  });
}

/** Soft-deletes an entry (status: 'deleted'); never shown in the UI again. */
export async function deleteItem(listId: string, entryId: string): Promise<void> {
  const now = new Date().toISOString();
  const mutation = await buildMutation(listId, entryId, 'delete_item', {});

  await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    await db.list_entries.update(entryId, {
      status: 'deleted',
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
  });
}

/**
 * Increases an entry's quantity by 1.
 *
 * Modeled on the canonical `add_item` dedup path: the server's `add_item`
 * handler increments `quantity` when it sees the existing `entity_id` (see
 * implementation-plan §P2-T5 "Merge quantity"). So we re-enqueue an `add_item`
 * mutation on the EXISTING entry id and optimistically bump the local count.
 */
export async function incrementQuantity(
  listId: string,
  entryId: string
): Promise<void> {
  const entry = await db.list_entries.get(entryId);
  if (!entry) return;

  const now = new Date().toISOString();
  // Build the mutation up front (it reads list_meta, which is outside the rw
  // transaction scope below) so the write stays atomic over the two tables.
  const mutation = await buildMutation(listId, entryId, 'add_item', {
    raw_text: entry.raw_text,
    display_name: entry.display_name,
    quantity: 1
  });

  await db.transaction('rw', db.list_entries, db.mutation_queue, async () => {
    const current = await db.list_entries.get(entryId);
    if (!current) return;
    await db.list_entries.update(entryId, {
      quantity: current.quantity + 1,
      updated_at: now
    });
    await db.mutation_queue.add(mutation);
  });
}

/**
 * Decreases an entry's quantity by 1 (floor of 1).
 *
 * TODO(contract): there is no canonical decrement operation_type in the enum
 * (add_item, mark_bought, mark_skipped, delete_item, rename_item, resolve_alias,
 * reject_match, set_constraint). We update locally only — no mutation is queued —
 * until the backend supports it. Flagged for the user in the P2-T3 report.
 */
export async function decrementQuantity(entryId: string): Promise<void> {
  await db.transaction('rw', db.list_entries, async () => {
    const entry = await db.list_entries.get(entryId);
    if (!entry || entry.quantity <= 1) return;
    await db.list_entries.update(entryId, {
      quantity: entry.quantity - 1,
      updated_at: new Date().toISOString()
    });
  });
}

/**
 * Pull-to-refresh / force-sync hook.
 *
 * TODO(P2-T4): the sync loop is out of scope for this task. This is a no-op
 * placeholder so the UI can wire pull-to-refresh now; P2-T4 fills it in with the
 * real queue-drain + snapshot fetch.
 */
export async function forceSync(_listId: string): Promise<void> {
  // intentionally empty — see TODO(P2-T4)
}
