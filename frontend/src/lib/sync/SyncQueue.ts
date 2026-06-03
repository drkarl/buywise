// Mutation queue drain.
//
// The P2-T3 store writes every user action into Dexie's `mutation_queue` with
// `status='pending'` and never touches the network. This module is the other
// half: when the app comes online it POSTs those pending mutations to
// `/api/mutations` in creation order, applies the server's authoritative entity
// back into `list_entries`, advances `LocalListMeta.current_revision`, and marks
// each mutation `confirmed` — or `failed` on a 4xx rejection (see plan P2-T4).
//
// Ordering matters: mutations are drained by ascending Dexie id (creation
// order) one at a time, because a later mutation can depend on an earlier one
// (e.g. mark_bought after add_item for the same entry). We do NOT parallelise.

import { db, type LocalMutation } from '$lib/db/dexie';
import { getDeviceId } from '$lib/db/device';
import { jsonMutationHeaders } from '$lib/api/csrf';
import { toLocalEntry, type MutationResponse } from './types';

/** Network-error retry budget per mutation (plan P2-T4). */
export const MAX_RETRIES = 3;

/** Base backoff for network-error retries; doubles each attempt. */
const RETRY_BASE_MS = 500;

/** A mutation the server rejected (4xx), surfaced to the UI. */
export interface FailedMutation {
  client_mutation_id: string;
  list_id: string;
  entity_id: string;
  operation_type: string;
  status: number;
  message: string;
}

export interface DrainResult {
  /** Mutations confirmed by the server this drain. */
  confirmed: number;
  /** Mutations the server rejected (4xx) and we marked `failed`. */
  failed: FailedMutation[];
}

export interface SyncQueueOptions {
  /** Override `fetch` (tests). Defaults to the global. */
  fetchFn?: typeof fetch;
  /** Override the backoff sleep (tests inject a no-op / fake timer). */
  sleep?: (ms: number) => Promise<void>;
  /** Called once per server-rejected mutation so the UI can surface it. */
  onFailed?: (failure: FailedMutation) => void;
}

function defaultSleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Thrown for transport-level failures (offline, DNS, 5xx) — retryable. */
class NetworkError extends Error {}

/**
 * POSTs a single mutation. Resolves with the parsed body on 2xx; throws
 * `NetworkError` on a transport failure or 5xx (retryable); returns a
 * `{ rejected }` marker on a 4xx (terminal — the server refused it).
 */
async function postMutation(
  fetchFn: typeof fetch,
  mutation: LocalMutation
): Promise<{ ok: MutationResponse } | { rejected: { status: number; message: string } }> {
  const body = {
    client_mutation_id: mutation.client_mutation_id,
    // Prefer the device id captured when the mutation was enqueued: the device
    // that created the mutation owns its attribution. Fall back to the live
    // `getDeviceId()` only when the queued value is missing (e.g. the row was
    // created before `getDeviceId` settled in an SSR/private-mode fallback).
    device_id: mutation.device_id || getDeviceId(),
    list_id: mutation.list_id,
    // `base_revision` is captured at enqueue and intentionally NOT re-stamped
    // before each POST. The server is pure last-write-wins (plan P1-T4: "the
    // server always accepts the mutation and assigns a new revision") and never
    // rejects on a stale `base_revision`, so a multi-mutation offline batch
    // carrying older base_revisions is harmless — the field is informational.
    base_revision: mutation.base_revision,
    entity_type: mutation.entity_type,
    entity_id: mutation.entity_id,
    operation_type: mutation.operation_type,
    payload: mutation.payload,
    created_at_client: mutation.created_at_client
  };

  let response: Response;
  try {
    response = await fetchFn('/api/mutations', {
      method: 'POST',
      // The backend rejects writes without a matching X-CSRF-Token (403). Echo
      // the JS-readable `csrf` cookie; if it's absent (unauthenticated) the 403
      // is a legitimate auth failure surfaced via the normal 4xx → onFailed path.
      headers: jsonMutationHeaders(),
      body: JSON.stringify(body)
    });
  } catch (cause) {
    // fetch only rejects on transport failure (offline, aborted, DNS).
    throw new NetworkError(cause instanceof Error ? cause.message : 'network error');
  }

  if (response.ok) {
    return { ok: (await response.json()) as MutationResponse };
  }
  if (response.status >= 500) {
    // Server-side transient failure — treat like a network error and retry.
    throw new NetworkError(`server error ${response.status}`);
  }
  // 4xx: the server refused this mutation; retrying will not help.
  let message = `request rejected (${response.status})`;
  try {
    const text = await response.text();
    if (text) message = text;
  } catch {
    // ignore — keep the generic message
  }
  return { rejected: { status: response.status, message } };
}

/**
 * Applies an accepted/merged mutation response to Dexie in one transaction:
 * replaces the local entry with the server's authoritative entity, advances the
 * list's `current_revision`, and marks the mutation `confirmed`. The revision is
 * advanced monotonically (`max`) so an out-of-order or replayed response can
 * never roll the baseline backwards.
 */
async function applySuccess(mutationId: number, res: MutationResponse): Promise<void> {
  const entry = toLocalEntry(res.entity);
  await db.transaction('rw', db.list_entries, db.list_meta, db.mutation_queue, async () => {
    // Per-entry monotonic guard (mirrors applyMutationEvent): a delayed POST
    // response can carry an older entity than one the SSE stream already applied
    // for the same entry. Only overwrite when the response is at least as new by
    // `last_updated_revision`, so a stale response can't clobber a newer row.
    const existing = await db.list_entries.get(entry.id);
    if (!existing || entry.last_updated_revision >= existing.last_updated_revision) {
      await db.list_entries.put(entry);
    }

    const meta = await db.list_meta.get(entry.list_id);
    if (meta) {
      if (res.server_revision > meta.current_revision) {
        await db.list_meta.update(entry.list_id, {
          current_revision: res.server_revision,
          last_synced_at: new Date().toISOString()
        });
      } else {
        await db.list_meta.update(entry.list_id, { last_synced_at: new Date().toISOString() });
      }
    } else {
      // First contact for this list (no GET /api/list/:id yet): seed meta from
      // the response so subsequent reconnects have a `?since=` baseline.
      await db.list_meta.put({
        id: entry.list_id,
        name: '',
        current_revision: res.server_revision,
        last_synced_at: new Date().toISOString()
      });
    }

    await db.mutation_queue.update(mutationId, { status: 'confirmed' });
  });
}

/**
 * Drains every `status='pending'` mutation for `listId` in creation order.
 *
 * For each mutation:
 *  - 2xx → apply the entity, advance the revision, mark `confirmed`.
 *  - transport failure / 5xx → retry up to {@link MAX_RETRIES} with exponential
 *    backoff (500ms → 1s → 2s); if still failing, leave it `pending` and stop
 *    the drain (preserves ordering — the next reconnect resumes from here).
 *  - 4xx → mark `failed`, surface to the caller, and continue with the rest.
 *
 * Returns counts of confirmed and the list of failed mutations.
 */
export async function drainQueue(
  listId: string,
  options: SyncQueueOptions = {}
): Promise<DrainResult> {
  const fetchFn = options.fetchFn ?? fetch;
  const sleep = options.sleep ?? defaultSleep;

  const pending = await db.mutation_queue
    .where('list_id')
    .equals(listId)
    .filter((m) => m.status === 'pending')
    .toArray();
  // `where('list_id')` does not guarantee id order; sort by the auto-increment
  // PK so we replay in true creation order.
  pending.sort((a, b) => (a.id ?? 0) - (b.id ?? 0));

  const result: DrainResult = { confirmed: 0, failed: [] };

  for (const mutation of pending) {
    if (mutation.id === undefined) continue; // defensive — persisted rows always have an id

    // `attempt` is the per-drain retry budget (bounded by MAX_RETRIES so a
    // mutation that exhausted retries in a past drain still retries on the next
    // reconnect). `retry_count` persisted on the row is the LIFETIME counter, so
    // we accumulate onto the value already stored rather than resetting it.
    let attempt = 0;
    let lifetimeRetries = mutation.retry_count ?? 0;

    // Retry loop for transport/5xx failures only. A 4xx breaks out via `failed`.
    for (;;) {
      let outcome: Awaited<ReturnType<typeof postMutation>>;
      try {
        outcome = await postMutation(fetchFn, mutation);
      } catch (err) {
        if (!(err instanceof NetworkError)) throw err;
        attempt += 1;
        lifetimeRetries += 1;
        await db.mutation_queue.update(mutation.id, { retry_count: lifetimeRetries });
        if (attempt > MAX_RETRIES) {
          // Out of retries while offline/degraded: leave this and all later
          // mutations `pending` and stop, so ordering is preserved for the
          // next reconnect.
          return result;
        }
        await sleep(RETRY_BASE_MS * 2 ** (attempt - 1));
        continue;
      }

      if ('ok' in outcome) {
        await applySuccess(mutation.id, outcome.ok);
        result.confirmed += 1;
      } else {
        await db.mutation_queue.update(mutation.id, { status: 'failed' });
        const failure: FailedMutation = {
          client_mutation_id: mutation.client_mutation_id,
          list_id: mutation.list_id,
          entity_id: mutation.entity_id,
          operation_type: mutation.operation_type,
          status: outcome.rejected.status,
          message: outcome.rejected.message
        };
        result.failed.push(failure);
        options.onFailed?.(failure);
      }
      break;
    }
  }

  return result;
}
