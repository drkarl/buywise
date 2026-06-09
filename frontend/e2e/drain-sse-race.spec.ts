// Drain vs SSE race (issue #63 scope: "Drain vs SSE race").
//
// The rev-5/rev-6 stale-reconcile scenario from PR #61 rounds 6–7: context A
// mutates offline while context B advances the list revision; A reconnects and
// its drain + snapshot reconcile race the live SSE backfill of B's newer edits.
// The invariant under test: NO rollback of B's newer edits and NO lost A edit —
// A's local state must converge forward to the latest server revision, never
// regress to the (older) reconcile snapshot.
//
// We make B's advance LARGER than A's single offline edit so a buggy wholesale
// reconcile over a stale snapshot would be observable as either a vanished B
// item (rolled back) or A's edit reverting.

import { test, expect } from '@playwright/test';
import {
  SEED,
  openList,
  addItem,
  markBought,
  expectDexieEntry,
  dexieEntries,
  serverEntries,
  dexieRevision,
  uniqueName,
  waitForDrained
} from './fixtures';

test.describe('drain vs SSE race', () => {
  test('A offline edit + B revision advance both survive reconnect (no rollback, no lost edit)', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      // Baseline: an active item both contexts know about (A will act on it offline).
      const shared = uniqueName();
      await addItem(a, shared);
      await expectDexieEntry(b, SEED.listId, shared, 'active');
      await expectDexieEntry(a, SEED.listId, shared, 'active');

      const revBeforeOffline = await dexieRevision(a, SEED.listId);

      // A goes offline and marks the shared item bought (a NON-add op — exercises
      // the snapshot reconcile's revert/wholesale path, not the add removal path).
      await ctxA.setOffline(true);
      await markBought(a, shared);

      // While A is offline, B advances the revision well past A's offline edit by
      // adding several items. Each is a committed server mutation bumping the rev.
      // Distinct nouns keep every add off the client-side duplicate-warning path.
      const bItems = [uniqueName(), uniqueName(), uniqueName()];
      for (const name of bItems) {
        await addItem(b, name);
        await expectDexieEntry(b, SEED.listId, name, 'active');
      }
      const serverRevAfterB = (await serverEntries(ctxB, SEED.listId)).reduce(
        (max, e) => Math.max(max, e.last_updated_revision),
        0
      );
      expect(serverRevAfterB).toBeGreaterThan(revBeforeOffline);

      // A reconnects: its queued mark_bought drains AND the live SSE backfills
      // B's three adds. The two paths race; the engine must converge forward.
      await ctxA.setOffline(false);

      // Let A's offline mutation finish draining (it first burns its retry budget
      // against the dead network) before asserting no-rollback / no-lost-edit.
      await waitForDrained(a, SEED.listId);

      // 1. A's offline edit is NOT lost: the shared item ends up `bought`.
      await expectDexieEntry(a, SEED.listId, shared, 'bought');

      // 2. B's newer edits are NOT rolled back: all three adds land in A's Dexie
      //    as active rows (a stale wholesale reconcile would have dropped them).
      for (const name of bItems) {
        await expectDexieEntry(a, SEED.listId, name, 'active');
      }

      // 3. A's local revision advanced forward to at least the server's latest —
      //    never regressed to the older reconcile snapshot.
      await expect
        .poll(async () => dexieRevision(a, SEED.listId), {
          message: 'A local revision did not advance to server latest',
          timeout: 15_000
        })
        .toBeGreaterThanOrEqual(serverRevAfterB);

      // 4. Full agreement on the four race items between A's Dexie and the server.
      const names = [shared, ...bItems];
      const statusByName = (rows: { display_name: string; status: string }[]) =>
        Object.fromEntries(
          rows.filter((r) => names.includes(r.display_name)).map((r) => [r.display_name, r.status])
        );
      const localA = statusByName(await dexieEntries(a, SEED.listId));
      const server = statusByName(await serverEntries(ctxB, SEED.listId));
      expect(localA).toEqual(server);
      expect(server).toEqual({
        [shared]: 'bought',
        [bItems[0]]: 'active',
        [bItems[1]]: 'active',
        [bItems[2]]: 'active'
      });
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });
});
