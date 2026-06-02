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

export class BuyWiseDB extends Dexie {
  list_entries!: Table<LocalListEntry, string>;
  mutation_queue!: Table<LocalMutation, number>;
  list_meta!: Table<LocalListMeta, string>;

  constructor() {
    super('buywise');
    this.version(1).stores({
      list_entries: 'id, list_id, status, canonical_item_id, last_updated_revision',
      mutation_queue: '++id, list_id, status, client_mutation_id',
      list_meta: 'id',
    });
  }
}

export const db = new BuyWiseDB();
