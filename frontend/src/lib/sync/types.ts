// Wire types and mappers for the sync engine.
//
// The server is authoritative (see design §11): every accepted mutation and
// every SSE `mutation` event carries the full server-side `list_entry` row, and
// the client replaces its local copy with it. These types describe the JSON
// shapes the server returns (per docs/implementation-plan.md P1-T3/T4/T5 and
// design §11) and `toLocalEntry` projects a server entry onto the local Dexie
// `LocalListEntry` shape, dropping server-only columns the client doesn't store
// (e.g. `added_by`, `effective_constraint_hash`, `resolution_candidates_json`).

import type { LocalListEntry } from '$lib/db/dexie';

/**
 * Server `list_entry` row as serialised by `GET /api/list/:id`, the
 * `POST /api/mutations` response `entity`, and the SSE `mutation` event
 * `entity`. A superset of `LocalListEntry`; only the fields we persist locally
 * are read by `toLocalEntry`.
 */
export interface ServerListEntry {
  id: string;
  list_id: string;
  canonical_item_id?: string | null;
  resolution_status: 'unresolved' | 'resolved' | 'ambiguous';
  raw_text: string;
  display_name: string;
  quantity: number;
  status: 'active' | 'bought' | 'skipped' | 'deleted';
  bought_by?: string | null;
  bought_at?: string | null;
  category_id?: string | null;
  last_updated_revision: number;
  updated_at: string;
  // Server-only columns the client does not store; declared for completeness.
  added_by?: string | null;
  effective_constraint_hash?: string;
  resolution_candidates_json?: string | null;
}

/** `POST /api/mutations` success response (design §11, plan P1-T4). */
export interface MutationResponse {
  client_mutation_id: string;
  status: 'accepted' | 'merged';
  server_revision: number;
  entity: ServerListEntry;
}

/** `GET /api/list/:id` response (plan P1-T3). `current_revision` is top-level. */
export interface ListSnapshot {
  id: string;
  name: string;
  current_revision: number;
  entries: ServerListEntry[];
}

/** SSE `mutation` event `data` payload (plan P1-T5, design §11). */
export interface SseMutationData {
  server_revision: number;
  entity: ServerListEntry;
}

/**
 * SSE `full_snapshot` event `data` payload. Sent when the client reconnects
 * with a `?since=` below the server's retention floor (plan P1-T5): the server
 * returns the complete list state instead of an incremental replay. Shares the
 * `GET /api/list/:id` shape.
 */
export type SseSnapshotData = ListSnapshot;

/**
 * Projects an authoritative server entry onto the local Dexie row, normalising
 * the SQL-ish `null` the server may send for optional columns to `undefined`
 * (which is how `LocalListEntry` and the P2-T3 store represent "absent").
 */
export function toLocalEntry(entity: ServerListEntry): LocalListEntry {
  return {
    id: entity.id,
    list_id: entity.list_id,
    canonical_item_id: entity.canonical_item_id ?? undefined,
    resolution_status: entity.resolution_status,
    raw_text: entity.raw_text,
    display_name: entity.display_name,
    quantity: entity.quantity,
    status: entity.status,
    bought_by: entity.bought_by ?? undefined,
    bought_at: entity.bought_at ?? undefined,
    category_id: entity.category_id ?? undefined,
    last_updated_revision: entity.last_updated_revision,
    updated_at: entity.updated_at
  };
}
