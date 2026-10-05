// Pure grouping/ordering for shopping mode (P2-T6, sub-categories from issue
// #101). No Dexie, Svelte, or DOM here so it is trivially unit-testable: it
// takes plain arrays in and returns plain groups out. The page layer feeds it
// live data from Dexie.
//
// Source of truth for field names: crates/core/src/domain.rs (`Category`,
// `Subcategory`, `Retailer`) and migrations/003_phase1_schema.sql
// (`store_category_orders`) + migrations/010_subcategories.sql (`subcategories`).

import type {
  LocalCategory,
  LocalListEntry,
  LocalStoreCategoryOrder,
  LocalSubcategory
} from '$lib/db/dexie';

/**
 * One section in shopping mode, with its items already grouped.
 *
 * A section is a sub-category when the item's catalogue entry carries one
 * (issue #101); otherwise it is the parent category. Entries whose item has no
 * resolvable grouping at all bucket under `UNCATEGORISED_ID`.
 */
export interface CategoryGroup {
  /**
   * Identity of this section: the sub-category id when split, else the parent
   * category id, else `UNCATEGORISED_ID`. Unique within a result, so it is safe
   * as a rendering key.
   */
  id: string;
  /**
   * The parent category id. `UNCATEGORISED_ID` when the entry has no resolvable
   * category. Drives the store-specific aisle ordering.
   */
  category_id: string;
  /** Set when this section is one sub-category of `category_id`. */
  subcategory_id?: string;
  /** The parent `LocalCategory`, or `undefined` for uncategorised items. */
  category?: LocalCategory;
  /** The matching `LocalSubcategory`, or `undefined` for unsplit sections. */
  subcategory?: LocalSubcategory;
  /** Entries in this section, preserving their incoming (entry) order. */
  items: LocalListEntry[];
  /** Active (not yet bought/skipped) items remaining in this section. */
  remaining: number;
}

/**
 * Synthetic category id used to bucket entries that carry no `category_id`
 * (e.g. still-unresolved items). They are always grouped last; see
 * `displayOrderFor`.
 */
export const UNCATEGORISED_ID = '__uncategorised__';

/**
 * Ordering tiers, compared before any numeric key. Banding by an explicit tier
 * (rather than offsetting one numeric key past another) keeps every key inside
 * the safe-integer range, so neither a large `default_order` nor a hostile
 * cached `display_order` can collapse distinct keys by overflowing
 * `Number.MAX_SAFE_INTEGER`.
 */
const enum OrderTier {
  /** Curated by the household for the selected store. */
  Curated = 0,
  /** Known category, but not curated for this store — uses `default_order`. */
  Default = 1,
  /** Unknown/uncategorised — always sorts last. */
  Fallback = 2
}

/** A composite ordering key: tier first, then the in-tier numeric key. */
interface OrderKey {
  tier: OrderTier;
  value: number;
}

/**
 * Resolves a category's composite ordering key for the selected store.
 *
 * Precedence (deterministic at every level):
 *   1. The store's `display_order` if the household curated this (store,
 *      category) pair — this is what the store selector switches between.
 *   2. Otherwise the category's own `default_order`.
 *   3. Otherwise (unknown/uncategorised) a fixed key, so it sorts last.
 *
 * The tier guarantees the precedence regardless of the magnitude of any stored
 * order, so we never add one band's range onto another's (which previously
 * risked overflowing `Number.MAX_SAFE_INTEGER`). Ties within a tier fall to the
 * numeric key, then to a `category_id` tiebreak at the call site, keeping the
 * sort stable and reproducible regardless of input order.
 */
function displayOrderFor(
  categoryId: string,
  storeOrder: Map<string, number>,
  defaultOrder: Map<string, number>
): OrderKey {
  const curated = storeOrder.get(categoryId);
  if (curated !== undefined) return { tier: OrderTier.Curated, value: curated };

  const fallback = defaultOrder.get(categoryId);
  if (fallback !== undefined) return { tier: OrderTier.Default, value: fallback };

  return { tier: OrderTier.Fallback, value: 0 };
}

/**
 * Sentinel ordering value for a section that has no sub-category of its own: the
 * parent-category fallback section sorts AFTER every sub-category of its parent
 * (and before the next parent category).
 */
const NO_SUBCATEGORY_ORDER = Number.MAX_SAFE_INTEGER;

/**
 * Groups list entries by sub-category when the catalogue supplies one, and by
 * category otherwise, then orders the sections by the selected store's aisle
 * ordering (`store_category_orders.display_order`) and the sub-category's
 * `default_order` within its parent.
 *
 * - Items keep their incoming order within a section (callers pass entries
 *   oldest-first, matching the main list view).
 * - A broad category (e.g. Pantry) splits into its sub-categories in
 *   `default_order`; an item whose entry carries no known sub-category falls
 *   back to a single section keyed by its parent category, so nothing
 *   disappears.
 * - The parent category drives store ordering: every sub-category of a curated
 *   category inherits that category's (store, display_order) slot. Per-store
 *   layouts for sub-categories are out of scope (issue #101).
 * - Categories not present in `orders` for `storeId` fall back to the category
 *   `default_order`, then to a stable id tiebreak; entries with no grouping at
 *   all are bucketed under `UNCATEGORISED_ID` and always sort last.
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
  categories: LocalCategory[] = [],
  subcategories: LocalSubcategory[] = []
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

  const subcategoryById = new Map<string, LocalSubcategory>();
  for (const s of subcategories) subcategoryById.set(s.id, s);

  // Bucket entries, preserving incoming order within each bucket. An entry with
  // a known sub-category buckets by it; anything else (no sub-category id, or a
  // stale id absent from the catalogue) buckets by parent category.
  const buckets = new Map<
    string,
    { category_id: string; subcategory_id?: string; items: LocalListEntry[] }
  >();
  for (const item of items) {
    const subcategory =
      item.subcategory_id === undefined ? undefined : subcategoryById.get(item.subcategory_id);
    const key = subcategory ? subcategory.id : (item.category_id ?? UNCATEGORISED_ID);
    const bucket = buckets.get(key);
    if (bucket) {
      bucket.items.push(item);
    } else {
      buckets.set(key, {
        category_id: subcategory ? subcategory.category_id : (item.category_id ?? UNCATEGORISED_ID),
        subcategory_id: subcategory?.id,
        items: [item]
      });
    }
  }

  const groups: CategoryGroup[] = [];
  for (const [id, bucket] of buckets) {
    groups.push({
      id,
      category_id: bucket.category_id,
      subcategory_id: bucket.subcategory_id,
      category: categoryById.get(bucket.category_id),
      subcategory:
        bucket.subcategory_id === undefined
          ? undefined
          : subcategoryById.get(bucket.subcategory_id),
      items: bucket.items,
      remaining: bucket.items.filter((i) => i.status === 'active').length
    });
  }

  // Order sections by the store key, then by sub-category display order within a
  // parent, with a stable id tiebreak so the result is fully deterministic.
  const orderValues = new Map<string, { order: OrderKey; suborder: number }>();
  for (const group of groups) {
    orderValues.set(group.id, {
      order: displayOrderFor(group.category_id, storeOrder, defaultOrder),
      suborder:
        group.subcategory_id === undefined
          ? NO_SUBCATEGORY_ORDER
          : (subcategoryById.get(group.subcategory_id)?.default_order ?? NO_SUBCATEGORY_ORDER)
    });
  }

  groups.sort((a, b) => {
    const ka = orderValues.get(a.id)!;
    const kb = orderValues.get(b.id)!;
    if (ka.order.tier !== kb.order.tier) return ka.order.tier - kb.order.tier;
    if (ka.order.value !== kb.order.value) return ka.order.value - kb.order.value;
    if (ka.suborder !== kb.suborder) return ka.suborder - kb.suborder;
    return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });

  return groups;
}
