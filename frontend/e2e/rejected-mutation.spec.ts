// Rejected mutation (issue #63 scope: "Rejected mutation").
//
// Force `POST /api/mutations` to return a 4xx and assert the sync engine:
//   1. surfaces the failure in the UI (the list view's `role="alert"` notice), and
//   2. reconciles the optimistic row away — a rejected `add_item` creates a bogus
//      client-side entity, so `runSync` removes it (revision-independent) and a
//      `GET /api/list/:id` reconcile confirms it never existed server-side.
//
// We intercept only the mutations POST (route interception) so the snapshot
// GET that drives the reconcile still hits the real backend.

import { test, expect } from '@playwright/test';
import { SEED, login, listPath, addItem, dexieEntries, activeRow, uniqueName } from './fixtures';

test.describe('rejected mutation reconciliation', () => {
  test('a 4xx on add_item reverts the optimistic row and surfaces the failure', async ({
    browser
  }) => {
    const ctx = await browser.newContext();
    try {
      await login(ctx);
      const page = await ctx.newPage();

      // Reject every mutation POST with a 422 (the backend's own code for an
      // unprocessable mutation). The body is echoed into the UI notice.
      await page.route('**/api/mutations', async (route) => {
        await route.fulfill({
          status: 422,
          contentType: 'text/plain',
          body: 'rejected by test'
        });
      });

      await page.goto(listPath);
      await expect(page.getByRole('heading', { name: 'Shopping list' })).toBeVisible();

      const item = uniqueName();
      await addItem(page, item);

      // The optimistic add is briefly visible (queued + optimistic store write).
      // After the drain rejects and runSync reconciles, the row is gone.
      await expect
        .poll(
          async () => {
            const entries = await dexieEntries(page, SEED.listId);
            return entries.some((e) => e.display_name === item);
          },
          { message: 'rejected add was not reconciled out of Dexie', timeout: 15_000 }
        )
        .toBeFalsy();
      await expect(activeRow(page, item)).toHaveCount(0);

      // The failure is surfaced to the user (assertive alert with the verb +
      // the server message).
      const alert = page.getByRole('alert');
      await expect(alert).toBeVisible();
      await expect(alert).toContainText('add');
      await expect(alert).toContainText('rejected by test');

      // The server never saw the entity: the authoritative snapshot omits it.
      const res = await ctx.request.get(`/api/list/${SEED.listId}`);
      const body = (await res.json()) as { entries: Array<{ display_name: string }> };
      expect(body.entries.some((e) => e.display_name === item)).toBeFalsy();
    } finally {
      await ctx.close();
    }
  });
});
