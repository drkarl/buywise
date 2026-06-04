import Dexie, { type Table } from 'dexie';

export interface LocalListEntry {
  id: string;                    // UUID v4; matches server entry.id
  list_id: string;
  canonical_item_id?: string;
  resolution_status: 'unresolved' | 'resolved' | 'ambiguous';
  raw_text: string;
  display_name: string;
  quantity: number;
  status: 'active' | 'bought' | 'skipped' | 'deleted';
  bought_by?: string;
  bought_at?: string;
  category_id?: string;
  last_updated_revision: number;
  updated_at: string;
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

export class BuyWiseDB extends Dexie {
  list_entries!: Table<LocalListEntry, string>;
  mutation_queue!: Table<LocalMutation, number>;
  list_meta!: Table<LocalListMeta, string>;
  categories!: Table<LocalCategory, string>;
  store_category_orders!: Table<LocalStoreCategoryOrder, string>;

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
    // and by the compound `[store+category_id]` (mirrors the server's UNIQUE
    // constraint for idempotent upserts).
    this.version(2).stores({
      categories: 'id, default_order',
      store_category_orders: 'id, store, [store+category_id]',
    });
  }
}

export const db = new BuyWiseDB();
