import Dexie, { type Table } from 'dexie';

export interface LocalListEntry {
  id: string;                    // UUID v4; matches server entry.id
  list_id: string;
  canonical_item_id?: string;
  resolution_status: 'unresolved' | 'resolved' | 'ambiguous';
  // Candidate canonical_item_ids for an `ambiguous` entry, as the raw JSON array
  // string the server sends (e.g. '["ci-a","ci-b"]'). Present only while
  // `resolution_status === 'ambiguous'`; cleared once resolved. Drives the P3-T3
  // disambiguation modal. Not indexed (Dexie stores it as a plain field).
  resolution_candidates_json?: string;
  raw_text: string;
  display_name: string;
  quantity: number;
  status: 'active' | 'bought' | 'skipped' | 'deleted';
  bought_by?: string;
  bought_at?: string;
  category_id?: string;
  /** Sub-category id (issue #101); absent when the item has no split. */
  subcategory_id?: string;
  last_updated_revision: number;
  updated_at: string;
  /**
   * Whether the entry is resolved but has no usable product match, so no price
   * can be produced for it (issue #108); drives the "Needs match" prompt on the
   * list row. Older mutation/SSE payloads may omit it; local upserts preserve the
   * known value in that case.
   */
  needs_match?: boolean;
}

export interface LocalMutation {
  id?: number;                   // Dexie auto-increment PK
  client_mutation_id: string;    // UUID v4; idempotency key
  device_id: string;
  list_id: string;
  entity_type: string;
  entity_id: string;
  operation_type: string;
  payload: object;
  base_revision: number;
  created_at_client: string;
  status: 'pending' | 'sent' | 'confirmed' | 'failed';
  retry_count: number;
}

export interface LocalListMeta {
  id: string;                    // list_id
  name: string;
  current_revision: number;      // last known server revision
  last_synced_at?: string;
}

/**
 * A product category, mirroring the `categories` row in the Rust core
 * (crates/core/src/domain.rs::Category). Cached from the `GET /api/items`
 * response so shopping mode can render category names + icons and fall back to
 * `default_order` for any (store, category) pair the household has not curated.
 */
export interface LocalCategory {
  id: string;                    // category_id; matches store_category_orders.category_id
  name_en: string;
  name_es: string;
  icon: string;
  default_order: number;
}

/**
 * An optional sub-category beneath a `LocalCategory` (issue #101), mirroring the
 * `subcategories` row in the Rust core (crates/core/src/domain.rs::Subcategory).
 * Shopping mode splits broad categories (e.g. Pantry) into these; `default_order`
 * orders the split within `category_id`. Shared reference data like
 * `LocalCategory`, cached from the `GET /api/items` response.
 */
export interface LocalSubcategory {
  id: string;                    // subcategory_id
  category_id: string;           // parent category id
  name_en: string;
  name_es: string;
  default_order: number;
}

/**
 * One per-household, per-store aisle-ordering row, mirroring the
 * `store_category_orders` table (migrations/003_phase1_schema.sql). Cached from
 * `GET /api/items` so shopping mode can re-order categories per store fully
 * offline. `store` is a Retailer DB string (tesco | dunnes | supervalu | aldi |
 * lidl); see crates/core/src/domain.rs::Retailer::as_db_str.
 */
export interface LocalStoreCategoryOrder {
  id: string;                    // server row id (e.g. "sco-tesco-<category_id>")
  store: string;                 // tesco | dunnes | supervalu | aldi | lidl
  category_id: string;
  display_order: number;
}

/**
 * One canonical-item → category mapping, cached from the `GET /api/items`
 * response (P2-T6). List entries only carry a (nullable) `canonical_item_id`;
 * shopping mode buckets on `category_id`, so we cache this lookup to resolve a
 * synced entry's category fully offline. Keyed by `canonical_item_id` so the
 * sync apply path can `get(canonical_item_id)` directly.
 */
export interface LocalItemCategory {
  canonical_item_id: string;     // canonical_items.id (the lookup key)
  category_id: string;           // canonical_items.category_id
  // canonical_items.subcategory_id (issue #101); absent when the item has no
  // sub-category, in which case shopping mode groups it by `category_id`.
  subcategory_id?: string;
  // The canonical item's English display name, cached so the P3-T3 ambiguity
  // modal can label candidate ids (which are all the local entry carries).
  // Optional: entries cached before this field existed simply have no name and
  // fall back to the id. Not indexed.
  name_en?: string;
}

export class BuyWiseDB extends Dexie {
  list_entries!: Table<LocalListEntry, string>;
  mutation_queue!: Table<LocalMutation, number>;
  list_meta!: Table<LocalListMeta, string>;
  categories!: Table<LocalCategory, string>;
  subcategories!: Table<LocalSubcategory, string>;
  store_category_orders!: Table<LocalStoreCategoryOrder, string>;
  item_categories!: Table<LocalItemCategory, string>;

  constructor() {
    super('buywise');
    this.version(1).stores({
      list_entries: 'id, list_id, status, canonical_item_id, last_updated_revision',
      mutation_queue: '++id, list_id, status, client_mutation_id',
      list_meta: 'id',
    });
    // v2 (P2-T6 shopping mode): cache the category catalogue and the per-store
    // aisle orders from `GET /api/items`. Additive migration — Dexie carries the
    // existing v1 stores forward untouched, so no upgrade function is needed.
    // `store_category_orders` is indexed by `store` (the shopping-mode lookup)
    // and by the compound `[store+category_id]` for idempotent upserts. The
    // server's UNIQUE constraint is `(household_id, store, category_id)`
    // (migrations/003_phase1_schema.sql); this cache is single-household, so
    // `household_id` is implicit and the index scopes only the (store,
    // category_id) pair.
    this.version(2).stores({
      categories: 'id, default_order',
      store_category_orders: 'id, store, [store+category_id]',
    });
    // v3 (P2-T6): cache the canonical_item_id → category_id mapping from
    // `GET /api/items` so the sync apply path can resolve a synced entry's
    // category offline (list entries carry only `canonical_item_id`). Additive,
    // like v2 — Dexie carries the v1/v2 stores forward untouched, so no upgrade
    // function is needed. Keyed by `canonical_item_id` for direct lookup.
    this.version(3).stores({
      item_categories: 'canonical_item_id',
    });
    // v4 (issue #101): cache the shared `subcategories` catalogue (parent +
    // display order) from `GET /api/items` so shopping mode can split a broad
    // category offline. Additive, like v2/v3 — Dexie carries the earlier stores
    // forward untouched, so no upgrade function is needed. Indexed by
    // `category_id` (the split lookup) and `default_order` (ordering within a
    // parent).
    this.version(4).stores({
      subcategories: 'id, category_id, default_order',
    });
  }
}

export const db = new BuyWiseDB();
