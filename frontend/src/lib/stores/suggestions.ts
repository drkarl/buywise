// Purchase-suggestion store (issue #106).
//
// The server computes what the household is likely to need from its check-off
// history; this store fetches that list for one list id and exposes confirm
// (add to the list) and dismiss (push the next suggestion later) through the
// normal offline mutation queue. Confirming and dismissing both remove the row
// from this store immediately, so the Suggested section reacts without waiting
// for the server round-trip, and the queued mutation shows in the sync
// indicator like any other edit.
//
// Suggestion state is deliberately NOT stored in Dexie: it is derived server
// data, refreshed on mount and after each successful sync. Locally handled ids
// are remembered for the session so a refresh that races a not-yet-drained
// confirm/dismiss cannot resurrect a row.

import { writable, type Readable, type Writable } from 'svelte/store';
import { addCanonicalItem, dismissSuggestion } from '$lib/stores/list';

/** One suggested canonical item, mirroring `GET /api/list/{id}/suggestions`. */
export interface Suggestion {
  canonical_item_id: string;
  display_name: string;
  typical_interval_days: number;
  last_purchased_on: string;
  days_since_last_purchase: number;
}

export interface SuggestionsHandle extends Readable<Suggestion[]> {
  /** Fetch the current suggestions, keeping the last list on failure (offline). */
  refresh(): Promise<void>;
  /** Add the item to the list and hide the suggestion immediately. */
  confirm(suggestion: Suggestion): Promise<void>;
  /** Dismiss the suggestion and hide it immediately. */
  dismiss(suggestion: Suggestion): Promise<void>;
}

/** Raw response body of `GET /api/list/{id}/suggestions`. */
interface SuggestionsResponse {
  suggestions?: Suggestion[];
}

/**
 * Build a suggestion store for `listId`. `fetchFn` is injectable for tests.
 */
export function createSuggestionsStore(
  listId: string,
  fetchFn: typeof fetch = fetch
): SuggestionsHandle {
  const store: Writable<Suggestion[]> = writable([]);
  // Ids the user confirmed/dismissed this session. A refresh before the queued
  // mutation drains must not bring them back.
  const handled = new Set<string>();

  async function refresh(): Promise<void> {
    let response: Response;
    try {
      response = await fetchFn(`/api/list/${encodeURIComponent(listId)}/suggestions`);
    } catch {
      return; // offline — keep whatever we have
    }
    if (!response.ok) return;
    let body: SuggestionsResponse;
    try {
      body = (await response.json()) as SuggestionsResponse;
    } catch {
      return;
    }
    const list = Array.isArray(body.suggestions) ? body.suggestions : [];
    store.set(list.filter((s) => !handled.has(s.canonical_item_id)));
  }

  function removeLocally(canonicalItemId: string): void {
    handled.add(canonicalItemId);
    store.update((items) => items.filter((s) => s.canonical_item_id !== canonicalItemId));
  }

  return {
    subscribe: store.subscribe,
    refresh,
    async confirm(suggestion) {
      removeLocally(suggestion.canonical_item_id);
      await addCanonicalItem(listId, suggestion.canonical_item_id, suggestion.display_name);
    },
    async dismiss(suggestion) {
      removeLocally(suggestion.canonical_item_id);
      await dismissSuggestion(listId, suggestion.canonical_item_id);
    }
  };
}
