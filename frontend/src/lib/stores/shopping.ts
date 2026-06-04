// Live Dexie stores for shopping mode (P2-T6). Read-only views over the cached
// catalogue (categories + per-store aisle orders) and the list entries shown
// while shopping. Mutations (check off / skip) reuse the existing `list.ts`
// store, so no write logic lives here.
//
// Everything renders straight off Dexie `liveQuery`, so shopping mode is fully
// offline-capable: the store selector, grouping, and progress bar all update
// without any network round-trip.

import { liveQuery, type Observable } from 'dexie';
import { readable, type Readable } from 'svelte/store';
import {
  db,
  type LocalCategory,
  type LocalListEntry,
  type LocalStoreCategoryOrder
} from '$lib/db/dexie';

/**
 * The five retailers BuyWise tracks, in canonical DB-string order. Mirrors
 * crates/core/src/domain.rs::Retailer::as_db_str — these ids must match the
 * `store` column of `store_category_orders`. Do not invent ids here.
 */
export const RETAILERS = [
  { id: 'tesco', label: 'Tesco' },
  { id: 'dunnes', label: 'Dunnes' },
  { id: 'supervalu', label: 'SuperValu' },
  { id: 'aldi', label: 'Aldi' },
  { id: 'lidl', label: 'Lidl' }
] as const;

/** A valid retailer/store id (the `store` value in `store_category_orders`). */
export type RetailerId = (typeof RETAILERS)[number]['id'];

const DEFAULT_STORE: RetailerId = 'tesco';

/** True when `value` is one of the five known retailer ids. */
export function isRetailerId(value: string): value is RetailerId {
  return RETAILERS.some((r) => r.id === value);
}

/**
 * Bridges a Dexie `liveQuery` Observable to a Svelte store. Mirrors the helper
 * in `list.ts`: the query re-runs whenever a table it touches changes, so
 * shopping mode stays reactive as items are checked off.
 */
function liveStore<T>(querier: () => T | Promise<T>, initial: T): Readable<T> {
  const observable: Observable<T> = liveQuery(querier);
  return readable<T>(initial, (set) => {
    const subscription = observable.subscribe({
      next: (value) => set(value),
      error: (err) => console.error('[shopping store] liveQuery error', err)
    });
    return () => subscription.unsubscribe();
  });
}

/**
 * Entries shown in shopping mode for a list: active and bought, oldest first.
 * Skipped and deleted entries are excluded — skipped items are not part of the
 * shop, and deleted entries are never surfaced (mirrors `list.ts`).
 *
 * Bought entries are kept (not filtered out) so they render struck-through in
 * place and so the progress bar can count `bought / (active + bought)`.
 */
export function shoppingEntries(listId: string): Readable<LocalListEntry[]> {
  return liveStore<LocalListEntry[]>(
    () =>
      db.list_entries
        .where('list_id')
        .equals(listId)
        .filter((e) => e.status === 'active' || e.status === 'bought')
        .toArray(),
    []
  );
}

/** The cached category catalogue (names, icons, default order). */
export function categories(): Readable<LocalCategory[]> {
  return liveStore<LocalCategory[]>(() => db.categories.toArray(), []);
}

/** All cached per-store aisle orders across every store. */
export function storeCategoryOrders(): Readable<LocalStoreCategoryOrder[]> {
  return liveStore<LocalStoreCategoryOrder[]>(
    () => db.store_category_orders.toArray(),
    []
  );
}

/**
 * Persists the user's selected store across sessions/reloads. localStorage is
 * synchronous and offline-safe; guarded so SSR/test environments without a DOM
 * degrade to the default rather than throwing.
 */
const STORE_KEY = 'buywise:shopping:store';

export function loadSelectedStore(): RetailerId {
  try {
    const saved = globalThis.localStorage?.getItem(STORE_KEY);
    if (saved && isRetailerId(saved)) return saved;
  } catch {
    // localStorage unavailable (private mode / SSR) — fall through to default.
  }
  return DEFAULT_STORE;
}

export function saveSelectedStore(store: RetailerId): void {
  try {
    globalThis.localStorage?.setItem(STORE_KEY, store);
  } catch {
    // Non-fatal: selection simply won't persist this session.
  }
}
