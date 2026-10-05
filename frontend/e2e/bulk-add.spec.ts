// Bulk-add from a pasted list (issue #115): paste → preview → edit/remove →
// confirm, and the offline → reconnect case proving a batch is never queued (or
// replayed) twice.
//
// The suite drives the real SPA + backend on one origin (playwright.config.ts).
// State assertions read the page's own Dexie/IndexedDB and the server snapshot,
// matching the other sync specs — not a UI re-render proxy.

import { test, expect, type Page } from '@playwright/test';
import {
  SEED,
  openList,
  expectDexieEntry,
  dexieEntries,
  serverEntries,
  uniqueName,
  waitForDrained
} from './fixtures';

/** Opens the bulk-add dialog from the list view. */
async function openBulkDialog(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'Paste a list' }).click();
  await expect(page.getByRole('dialog', { name: 'Paste a grocery list' })).toBeVisible();
}

/** Pastes `text` into the bulk dialog and waits for the preview rows to render. */
async function pasteList(page: Page, text: string): Promise<void> {
  await page.getByLabel('Grocery list text').fill(text);
  await expect(page.getByLabel('Item 1 text')).toBeVisible();
}

test.describe('bulk add from pasted list', () => {
  test('paste → edit quantity → remove → confirm queues only the confirmed entries', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      const first = uniqueName();
      const second = uniqueName();
      const removed = uniqueName();

      await openBulkDialog(a);
      await pasteList(a, `${first}\n${second}\n${removed}`);

      // One editable proposed entry per pasted line.
      await expect(a.getByLabel('Item 1 text')).toHaveValue(first);
      await expect(a.getByLabel('Item 2 text')).toHaveValue(second);
      await expect(a.getByLabel('Item 3 text')).toHaveValue(removed);

      // Nothing is added until Confirm.
      expect((await dexieEntries(a, SEED.listId)).some((e) => e.display_name === first)).toBeFalsy();

      // Edit a quantity and remove a proposed entry before confirming.
      await a.getByLabel('Item 1 quantity').fill('2');
      await a.getByRole('button', { name: 'Remove item 3' }).click();
      await expect(a.getByLabel('Item 3 text')).toHaveCount(0);

      await a.getByRole('button', { name: 'Add 2 items' }).click();
      await expect(a.getByRole('dialog')).toHaveCount(0);

      // The two confirmed entries are queued locally and drain to the server.
      await expectDexieEntry(a, SEED.listId, first, 'active');
      await expectDexieEntry(a, SEED.listId, second, 'active');
      const removedEntries = (await dexieEntries(a, SEED.listId)).filter(
        (e) => e.display_name === removed
      );
      expect(removedEntries).toHaveLength(0);

      await waitForDrained(a, SEED.listId);

      // The edited quantity followed the normal add-item rule.
      await expect
        .poll(async () => {
          const [row] = (await dexieEntries(a, SEED.listId)).filter((e) => e.display_name === first);
          return row?.quantity;
        })
        .toBe(2);

      // Server truth agrees, and only one add_item was queued per confirmed line.
      const server = (await serverEntries(ctxB, SEED.listId)).filter((e) =>
        [first, second, removed].includes(e.display_name)
      );
      expect(server.map((e) => [e.display_name, e.quantity, e.status]).sort()).toEqual(
        [
          [first, 2, 'active'],
          [second, 1, 'active']
        ].sort()
      );

      const addMutations = (await a.evaluate(
        () =>
          new Promise<Array<{ name: string; cmid: string; status: string }>>((resolve, reject) => {
            const req = indexedDB.open('buywise');
            req.onsuccess = () => {
              const db = req.result;
              const tx = db.transaction('mutation_queue', 'readonly');
              const out: Array<{ name: string; cmid: string; status: string }> = [];
              const cursorReq = tx.objectStore('mutation_queue').openCursor();
              cursorReq.onsuccess = () => {
                const cursor = cursorReq.result;
                if (cursor) {
                  const v = cursor.value as {
                    operation_type: string;
                    client_mutation_id: string;
                    status: string;
                    payload?: { display_name?: string };
                  };
                  if (v.operation_type === 'add_item' && v.payload?.display_name) {
                    out.push({
                      name: v.payload.display_name,
                      cmid: v.client_mutation_id,
                      status: v.status
                    });
                  }
                  cursor.continue();
                } else {
                  db.close();
                  resolve(out);
                }
              };
              cursorReq.onerror = () => reject(cursorReq.error);
            };
            req.onerror = () => reject(req.error);
          })
      ));
      const relevant = addMutations.filter((m) => [first, second].includes(m.name));
      expect(relevant).toHaveLength(2);
      expect(new Set(relevant.map((m) => m.cmid)).size).toBe(2);
      expect(relevant.every((m) => m.status === 'confirmed')).toBe(true);

      // The second device converges on the same two entries.
      await expectDexieEntry(b, SEED.listId, first, 'active');
      await expectDexieEntry(b, SEED.listId, second, 'active');
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });

  test('confirming offline queues the batch and reconnect/reload never adds it twice', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      await openList(ctxB);

      // Record every POST /api/mutations so we can prove one idempotency key per
      // entry (a new key per retry would let the server add duplicates).
      const sends: Array<{ cmid: string }> = [];
      await a.route('**/api/mutations', async (route) => {
        const body = route.request().postDataJSON() as { client_mutation_id: string };
        sends.push({ cmid: body.client_mutation_id });
        await route.continue();
      });

      const one = uniqueName();
      const two = uniqueName();

      // Go offline: confirming can only queue locally.
      await ctxA.setOffline(true);
      await openBulkDialog(a);
      await pasteList(a, `${one}\n${two}`);
      await a.getByRole('button', { name: 'Add 2 items' }).click();
      await expect(a.getByRole('dialog')).toHaveCount(0);

      // Optimistic rows exist locally while nothing has reached the server.
      await expectDexieEntry(a, SEED.listId, one, 'active');
      await expectDexieEntry(a, SEED.listId, two, 'active');
      const serverWhileOffline = await serverEntries(ctxB, SEED.listId);
      for (const name of [one, two]) {
        expect(serverWhileOffline.some((e) => e.display_name === name)).toBeFalsy();
      }

      // Reconnect → the queued batch drains (after burning its offline retries).
      await ctxA.setOffline(false);
      await waitForDrained(a, SEED.listId);

      // Each entry landed exactly once on the server.
      const serverAfter = (await serverEntries(ctxB, SEED.listId)).filter((e) =>
        [one, two].includes(e.display_name)
      );
      expect(serverAfter.map((e) => e.display_name).sort()).toEqual([one, two].sort());
      expect(serverAfter.every((e) => e.status === 'active')).toBe(true);

      // The drain replayed each mutation under a single stable idempotency key —
      // retries never minted a new one.
      expect(new Set(sends.map((s) => s.cmid)).size).toBe(2);

      // A reload re-runs the sync engine (nothing left pending) and must not add
      // the batch a second time.
      await a.goto('/list/' + SEED.listId);
      await expect(a.getByRole('heading', { name: 'Shopping list' })).toBeVisible();
      await waitForDrained(a, SEED.listId);
      const serverAfterReload = (await serverEntries(ctxB, SEED.listId)).filter((e) =>
        [one, two].includes(e.display_name)
      );
      expect(serverAfterReload.map((e) => e.display_name).sort()).toEqual([one, two].sort());
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });
});
