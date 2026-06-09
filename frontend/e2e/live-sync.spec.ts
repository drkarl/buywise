// Live sync (SSE) across two contexts (issue #63 scope: "Live sync").
//
// An edit made in context A must appear in context B within ~1s, delivered by
// the live SSE stream (no manual refresh in B). Covers add / check-off / delete
// so all three propagate as `mutation` events.

import { test, expect } from '@playwright/test';
import {
  SEED,
  openList,
  addItem,
  markBought,
  deleteItem,
  expectDexieEntry,
  activeRow,
  uniqueName
} from './fixtures';

test.describe('live sync over SSE', () => {
  test('add in A appears in B; check-off and delete propagate too', async ({ browser }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      const item = uniqueName();

      // A adds → B sees it live (SSE mutation event upserts B's Dexie + DOM).
      await addItem(a, item);
      await expectDexieEntry(b, SEED.listId, item, 'active');
      await expect(activeRow(b, item)).toBeVisible();

      // A checks it off → B's copy moves to `bought` (leaves the active list).
      await markBought(a, item);
      await expectDexieEntry(b, SEED.listId, item, 'bought');
      await expect(activeRow(b, item)).toHaveCount(0);

      // A second item, then delete it in A → B sees the tombstone (status
      // `deleted`) and the row leaves B's active list.
      const item2 = uniqueName();
      await addItem(a, item2);
      await expectDexieEntry(b, SEED.listId, item2, 'active');
      await deleteItem(a, item2);
      await expectDexieEntry(b, SEED.listId, item2, 'deleted');
      await expect(activeRow(b, item2)).toHaveCount(0);
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });
});
