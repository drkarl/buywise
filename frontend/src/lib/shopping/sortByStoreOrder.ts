// Pure grouping/ordering for shopping mode (P2-T6). No Dexie, Svelte, or DOM
// here so it is trivially unit-testable: it takes plain arrays in and returns
// plain category groups out. The page layer feeds it live data from Dexie.
//
// Source of truth for field names: crates/core/src/domain.rs (`Category`,
// `Retailer`) and migrations/003_phase1_schema.sql (`store_category_orders`).

import type {
  LocalCategory,
  LocalListEntry,
  LocalStoreCategoryOrder
} from '$lib/db/dexie';

/** One category section in shopping mode, with its items already grouped. */
export interface CategoryGroup {
  category_id: string;
  /** The matching `LocalCategory`, or `undefined` for uncategorised items. */
  category?: LocalCategory;
  /** Entries in this category, preserving their incoming (entry) order. */
  items: LocalListEntry[];
  /** Active (not yet bought/skipped) items remaining in this category. */
  remaining: number;
}

/**
 * Synthetic category id used to bucket entries that carry no `category_id`
 * (e.g. still-unresolved items). They are always grouped last; see
 * `displayOrderFor`.
 */
export const UNCATEGORISED_ID = '__uncategorised__';

// Categories absent from a store's curated order, and the uncategorised bucket,
// are pushed past every real `display_order` so they always sort last in a
// deterministic, store-independent way.
const FALLBACK_BASE = Number.MAX_SAFE_INTEGER;

/**
 * Resolves a category's effective ordering key for the selected store.
 *
 * Precedence (deterministic at every level):
 *   1. The store's `display_order` if the household curated this (store,
 *      category) pair — this is what the store selector switches between.
 *   2. Otherwise the category's own `default_order`, offset past all real
 *      store orders so curated categories always precede uncurated ones.
 *   3. Otherwise (unknown/uncategorised) the maximum key, so it sorts last.
 *
 * Ties are broken by `category_id` at the call site, keeping the sort stable
 * and reproducible regardless of input order.
 */
function displayOrderFor(
  categoryId: string,
  storeOrder: Map<string, number>,
  defaultOrder: Map<string, number>
): number {
  const curated = storeOrder.get(categoryId);
  if (curated !== undefined) return curated;

  const fallback = defaultOrder.get(categoryId);
  if (fallback !== undefined) return FALLBACK_BASE - 1 + fallback;

  return FALLBACK_BASE;
}

/**
 * Groups list entries by category and orders the groups by the selected store's
 * aisle ordering (`store_category_orders.display_order`).
 *
 * - Items keep their incoming order within a category (callers pass entries
 *   oldest-first, matching the main list view).
 * - Categories not present in `orders` for `storeId` fall back to the category
 *   `default_order`, then to a stable `category_id` tiebreak; entries with no
 *   `category_id` are bucketed under `UNCATEGORISED_ID` and always sort last.
 * - Pure and total: empty inputs yield an empty array.
 *
 * The page layer is responsible for hiding zero-`remaining` groups (the
 * documented "hide empty categories" product decision); this function keeps
 * every non-empty group so checked-off items stay visible in place.
 */
export function sortByStoreOrder(
  items: LocalListEntry[],
  orders: LocalStoreCategoryOrder[],
  storeId: string,
  categories: LocalCategory[] = []
): CategoryGroup[] {
  // Per-store display_order lookup for the selected store only.
  const storeOrder = new Map<string, number>();
  for (const o of orders) {
    if (o.store === storeId) storeOrder.set(o.category_id, o.display_order);
  }

  const categoryById = new Map<string, LocalCategory>();
  const defaultOrder = new Map<string, number>();
  for (const c of categories) {
    categoryById.set(c.id, c);
    defaultOrder.set(c.id, c.default_order);
  }

  // Bucket entries by category, preserving incoming order within each bucket.
  const buckets = new Map<string, LocalListEntry[]>();
  for (const item of items) {
    const key = item.category_id ?? UNCATEGORISED_ID;
    const bucket = buckets.get(key);
    if (bucket) bucket.push(item);
    else buckets.set(key, [item]);
  }

  const groups: CategoryGroup[] = [];
  for (const [categoryId, bucketItems] of buckets) {
    groups.push({
      category_id: categoryId,
      category: categoryById.get(categoryId),
      items: bucketItems,
      remaining: bucketItems.filter((i) => i.status === 'active').length
    });
  }

  // Order groups by the store key, with a stable category_id tiebreak so the
  // result is fully deterministic for equal keys (and for the fallback bucket).
  groups.sort((a, b) => {
    const oa = displayOrderFor(a.category_id, storeOrder, defaultOrder);
    const ob = displayOrderFor(b.category_id, storeOrder, defaultOrder);
    if (oa !== ob) return oa - ob;
    return a.category_id < b.category_id ? -1 : a.category_id > b.category_id ? 1 : 0;
  });

  return groups;
}
