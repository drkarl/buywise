// Always-visible sync indicator (issue #113).
//
// The acceptance case: with the phone offline, adding an item shows a pending
// count (and a per-item marker), and reconnecting clears it once the server
// confirms. We read the indicator (a role="status" live region) and the page's
// own Dexie, driving the real backend exactly as the other sync specs do.

import { test, expect } from '@playwright/test';
import { SEED, openList, addItem, uniqueName, waitForDrained, activeRow } from './fixtures';

test.describe('sync status indicator', () => {
  test('offline add shows a pending count that clears after reconnect', async ({ browser }) => {
    const context = await browser.newContext();
    try {
      const page = await openList(context);
      const indicator = page.getByRole('status');

      // Online and settled → synced.
      await expect(indicator).toContainText('Synced');

      await context.setOffline(true);
      const name = uniqueName();
      await addItem(page, name);

      // The optimistic row is visible and marked as not yet synced.
      await expect(activeRow(page, name)).toBeVisible();
      await expect(page.getByRole('img', { name: 'Not synced yet' })).toBeVisible();

      // The indicator reports offline with exactly one queued change.
      await expect(indicator).toHaveAttribute('data-state', 'offline');
      await expect(indicator).toContainText('Offline — 1 change pending');

      // Reconnect drains the queue; once the server confirms, the count clears.
      await context.setOffline(false);
      await waitForDrained(page, SEED.listId);

      await expect(indicator).toHaveAttribute('data-state', 'synced', { timeout: 30_000 });
      await expect(indicator).toContainText('Synced');
      await expect(page.getByRole('img', { name: 'Not synced yet' })).toHaveCount(0);
    } finally {
      await context.close();
    }
  });
});
