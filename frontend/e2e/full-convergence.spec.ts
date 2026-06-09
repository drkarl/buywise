// Full convergence over the COMPLETE entry set (issue #63 scope:
// "Full convergence"; unblocked by #62, which made GET /api/list/:id carry
// every status incl. bought/skipped/deleted tombstones + list_id).
//
// After a mixed batch of mutations drains, each context's local Dexie
// `list_entries` must equal the authoritative `GET /api/list/:id` snapshot over
// ALL statuses — not just the active set. Because #62's snapshot is complete,
// the active/bought/skipped/deleted rows the contexts learned through the
// server (drain confirmations + SSE) match the snapshot exactly.
//
// NOTE: the client snapshot reconcile still PRESERVES purely-local non-active
// rows absent from the snapshot (a documented PR #61 back-compat workaround, see
// SseClient.applySnapshotEvent). This test only creates non-active rows THROUGH
// the server, so no such local-only rows exist and convergence is exact. A
// scenario that deliberately leaves a stale local-only tombstone would expose
// that known gap and is intentionally out of scope here.

import { test, expect } from '@playwright/test';
import {
  SEED,
  openList,
  addItem,
  markBought,
  markSkipped,
  deleteItem,
  expectDexieEntry,
  dexieEntries,
  serverEntries,
  uniqueName,
  type EntrySnapshot
} from './fixtures';

/** Compare two entry sets over the fields that must converge (ignores rows
 *  outside `names` so unrelated items from other specs don't perturb equality). */
function project(entries: EntrySnapshot[], names: string[]) {
  return entries
    .filter((e) => names.includes(e.display_name))
    .map((e) => ({
      display_name: e.display_name,
      status: e.status,
      quantity: e.quantity
    }));
}

test.describe('full convergence over the complete entry set', () => {
  test('local Dexie equals GET /api/list/:id across active/bought/skipped/deleted', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      // Distinct nouns so the client-side duplicate check never holds an add.
      const active = uniqueName();
      const bought = uniqueName();
      const skipped = uniqueName();
      const deleted = uniqueName();
      const names = [active, bought, skipped, deleted];

      // A creates one item destined for each terminal status; all drain online.
      for (const name of names) {
        await addItem(a, name);
        await expectDexieEntry(a, SEED.listId, name, 'active');
      }
      await markBought(a, bought);
      await markSkipped(a, skipped);
      await deleteItem(a, deleted);

      // Wait until A's local state reflects every terminal status (drain confirmed).
      await expectDexieEntry(a, SEED.listId, active, 'active');
      await expectDexieEntry(a, SEED.listId, bought, 'bought');
      await expectDexieEntry(a, SEED.listId, skipped, 'skipped');
      await expectDexieEntry(a, SEED.listId, deleted, 'deleted');

      // B learns the same complete state purely through SSE.
      await expectDexieEntry(b, SEED.listId, active, 'active');
      await expectDexieEntry(b, SEED.listId, bought, 'bought');
      await expectDexieEntry(b, SEED.listId, skipped, 'skipped');
      await expectDexieEntry(b, SEED.listId, deleted, 'deleted');

      // Convergence: each context's COMPLETE local entry set equals the server's
      // complete snapshot (every status), via #62's complete-state endpoint.
      const server = project(await serverEntries(ctxA, SEED.listId), names);
      const localA = project(await dexieEntries(a, SEED.listId), names);
      const localB = project(await dexieEntries(b, SEED.listId), names);

      expect(localA).toEqual(server);
      expect(localB).toEqual(server);

      // And the snapshot genuinely enumerates the non-active entries (the #62
      // fix): all four expected statuses are present in the server set.
      const statuses = new Set(server.map((e) => e.status));
      expect(statuses).toEqual(new Set(['active', 'bought', 'skipped', 'deleted']));
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });
});
