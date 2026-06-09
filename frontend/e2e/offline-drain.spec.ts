// Offline → reconnect drain (issue #63 scope: "Offline → reconnect drain").
//
// Context A goes offline, makes a batch of edits across all four mutation kinds
// (add_item, mark_bought, mark_skipped, delete_item), then reconnects. The
// queued mutations must drain IN CREATION ORDER (SyncQueue replays by ascending
// id), and A must converge with the authoritative server state observed in
// context B. We exercise dependent ordering on purpose: an add followed by a
// mark_bought / delete on the SAME entry only succeeds if the add drains first.

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
  activeRow,
  uniqueName,
  waitForDrained
} from './fixtures';

test.describe('offline → reconnect drain', () => {
  test('all four mutation kinds queue offline and drain in order on reconnect', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      // Distinct nouns so the client-side duplicate check never holds an add.
      const toBuy = uniqueName(); // add, then mark_bought (dependent on the add)
      const toSkip = uniqueName(); // add, then mark_skipped
      const toDelete = uniqueName(); // add, then delete (dependent on the add)
      const toKeep = uniqueName(); // add, stays active

      // A goes offline. Every edit below is optimistic-only and queues in Dexie.
      await ctxA.setOffline(true);

      await addItem(a, toBuy);
      await addItem(a, toSkip);
      await addItem(a, toDelete);
      await addItem(a, toKeep);
      // Optimistic rows are visible while offline (no network involved).
      await expect(activeRow(a, toKeep)).toBeVisible();

      await markBought(a, toBuy);
      await markSkipped(a, toSkip);
      await deleteItem(a, toDelete);

      // Nothing has reached the server yet: B (online) must NOT see these.
      const serverBeforeReconnect = await serverEntries(ctxB, SEED.listId);
      for (const name of [toBuy, toSkip, toDelete, toKeep]) {
        expect(serverBeforeReconnect.some((e) => e.display_name === name)).toBeFalsy();
      }

      // Reconnect → the `online` event drives the drain (in id/creation order).
      await ctxA.setOffline(false);

      // Wait for A's queue to fully drain before asserting convergence: each
      // offline mutation first burns its retry/backoff budget against the dead
      // network, so the reconnect drain settles asynchronously.
      await waitForDrained(a, SEED.listId);

      // B observes the drained, converged server truth via SSE: kept item active,
      // bought/skipped in basket, deleted as a tombstone.
      await expectDexieEntry(b, SEED.listId, toKeep, 'active');
      await expectDexieEntry(b, SEED.listId, toBuy, 'bought');
      await expectDexieEntry(b, SEED.listId, toSkip, 'skipped');
      await expectDexieEntry(b, SEED.listId, toDelete, 'deleted');

      // A's own local Dexie reflects the same statuses after the drain confirms.
      await expectDexieEntry(a, SEED.listId, toKeep, 'active');
      await expectDexieEntry(a, SEED.listId, toBuy, 'bought');
      await expectDexieEntry(a, SEED.listId, toSkip, 'skipped');
      await expectDexieEntry(a, SEED.listId, toDelete, 'deleted');

      // Ordering proof: the dependent mark_bought / delete could only have
      // applied server-side if their add drained first. Confirm the server's
      // final state for these four items matches A's local state.
      const serverFinal = (await serverEntries(ctxB, SEED.listId)).filter((e) =>
        [toBuy, toSkip, toDelete, toKeep].includes(e.display_name)
      );
      const localA = (await dexieEntries(a, SEED.listId)).filter((e) =>
        [toBuy, toSkip, toDelete, toKeep].includes(e.display_name)
      );
      const statusByName = (rows: typeof serverFinal) =>
        Object.fromEntries(rows.map((r) => [r.display_name, r.status]));
      expect(statusByName(localA)).toEqual(statusByName(serverFinal));
      expect(statusByName(serverFinal)).toEqual({
        [toKeep]: 'active',
        [toBuy]: 'bought',
        [toSkip]: 'skipped',
        [toDelete]: 'deleted'
      });
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });
});
