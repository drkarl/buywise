// Catalogue fetch + cache for shopping mode (P2-T6).
//
// `GET /api/items` returns an envelope { items, categories, store_category_orders }
// (crates/api/src/lists.rs::ItemsResponse). This module fetches it and writes the
// three reference caches shopping mode reads entirely offline:
//   - `categories`            → names, icons, default_order
//   - `store_category_orders` → per-store aisle ordering
//   - `item_categories`       → canonical_item_id → category_id (entry resolution)
//
// It is OFFLINE-SAFE: on any network/parse failure it returns false and leaves
// whatever is already cached untouched, so shopping mode still renders from the
// last good cache (AC: "View works with Dexie offline, no network required").
//
// After (re)writing `item_categories` it BACKFILLS `category_id` on already-stored
// list entries — handling the catalogue-after-entries ordering (the
// entries-after-catalogue ordering resolves itself in the SSE apply path). The
// server is authoritative for entries, so we only ever ADD a category to entries
// that don't have one; we never overwrite.

import {
  db,
  type LocalCategory,
  type LocalItemCategory,
  type LocalStoreCategoryOrder
} from '$lib/db/dexie';

/** One item in the `GET /api/items` envelope (only the fields we cache). */
interface ItemsResponseItem {
  id: string;
  category_id: string;
  name_en: string;
}

/** One category row in the `GET /api/items` envelope. */
interface ItemsResponseCategory {
  id: string;
  name_en: string;
  name_es: string;
  icon: string;
  default_order: number;
}

/** One store_category_orders row in the `GET /api/items` envelope. */
interface ItemsResponseStoreOrder {
  id: string;
  store: string;
  category_id: string;
  display_order: number;
}

/** The `GET /api/items` envelope shape (crates/api/src/lists.rs::ItemsResponse). */
interface ItemsResponse {
  items: ItemsResponseItem[];
  categories: ItemsResponseCategory[];
  store_category_orders: ItemsResponseStoreOrder[];
}

/** Narrowing guard for the envelope (a stray bare array / garbled body is rejected). */
function isItemsResponse(value: unknown): value is ItemsResponse {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    Array.isArray(v.items) &&
    Array.isArray(v.categories) &&
    Array.isArray(v.store_category_orders)
  );
}

/**
 * Fetches `GET /api/items` and refreshes the shopping-mode caches, then backfills
 * `category_id` on stored entries. Returns true on a successful refresh, false if
 * the fetch failed (offline / non-OK / malformed) — in which case the existing
 * cache is left intact so the view still renders.
 *
 * The GET is served by the P2-T2 service worker (NetworkFirst), so a plain fetch
 * is fine: online hits the network, offline falls back to the SW cache, and only
 * a true cold-miss returns false.
 */
export async function fetchCatalogue(fetchFn: typeof fetch = fetch): Promise<boolean> {
  let response: Response;
  try {
    response = await fetchFn('/api/items');
  } catch {
    return false; // offline — keep the cached catalogue
  }
  if (!response.ok) return false;

  let body: unknown;
  try {
    body = await response.json();
  } catch {
    return false; // garbled body — don't clobber the cache
  }
  if (!isItemsResponse(body)) return false;

  await writeCatalogue(body);
  await backfillEntryCategories();
  return true;
}

/**
 * Replaces the cached `categories`, `store_category_orders`, and `item_categories`
 * with the envelope's contents. A full replace (clear + bulkPut) keeps the cache
 * in lock-step with the server: a category/order/item removed server-side does
 * not linger locally. All three writes share one transaction so a partial failure
 * never leaves the caches inconsistent.
 */
async function writeCatalogue(body: ItemsResponse): Promise<void> {
  const categories: LocalCategory[] = body.categories.map((c) => ({
    id: c.id,
    name_en: c.name_en,
    name_es: c.name_es,
    icon: c.icon,
    default_order: c.default_order
  }));
  const orders: LocalStoreCategoryOrder[] = body.store_category_orders.map((o) => ({
    id: o.id,
    store: o.store,
    category_id: o.category_id,
    display_order: o.display_order
  }));
  const itemCategories: LocalItemCategory[] = body.items.map((i) => ({
    canonical_item_id: i.id,
    category_id: i.category_id,
    name_en: i.name_en
  }));

  const tables = [db.categories, db.store_category_orders, db.item_categories] as const;
  await db.transaction('rw', tables, async () => {
    await db.categories.clear();
    await db.categories.bulkPut(categories);
    await db.store_category_orders.clear();
    await db.store_category_orders.bulkPut(orders);
    await db.item_categories.clear();
    await db.item_categories.bulkPut(itemCategories);
  });
}

/**
 * Backfills `category_id` on stored list entries from the freshly-written
 * `item_categories` cache. Handles the catalogue-after-entries ordering: an entry
 * synced before the catalogue arrived has a `canonical_item_id` but no
 * `category_id`, so we resolve it now. We only touch entries that have a
 * `canonical_item_id` but no `category_id` (never overwrite a server-set one),
 * and only when the cache actually resolves the item.
 */
async function backfillEntryCategories(): Promise<void> {
  await db.transaction('rw', db.list_entries, db.item_categories, async () => {
    const unresolved = await db.list_entries
      .filter((e) => e.canonical_item_id !== undefined && e.category_id === undefined)
      .toArray();
    if (unresolved.length === 0) return;

    const updates: { id: string; category_id: string }[] = [];
    for (const entry of unresolved) {
      const mapping = await db.item_categories.get(entry.canonical_item_id as string);
      if (mapping) updates.push({ id: entry.id, category_id: mapping.category_id });
    }
    for (const u of updates) {
      await db.list_entries.update(u.id, { category_id: u.category_id });
    }
  });
}
