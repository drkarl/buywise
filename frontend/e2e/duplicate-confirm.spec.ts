// Duplicate-warning "Add anyway" enqueue path (issue #83).
//
// The rest of the sync-loop suite deliberately uses mutually-dissimilar grocery
// nouns (see fixtures.ts `uniqueName` / NOUNS) so every add stays on the fast,
// no-prompt path and never trips the client-side duplicate detector
// (matching/duplicate.ts). That left the inline DuplicateWarning flow —
// AddItemInput.svelte holding an add behind the warning, then "Add anyway" ->
// enqueue, or "Cancel" -> discard — without e2e coverage. This spec exercises it.
//
// To trip the detector DETERMINISTICALLY we re-add the EXACT same text as a
// live item: `checkForDuplicate` normalises both sides identically, so the
// `exact` branch (confidence 1.0) matches immediately — no reliance on the
// fuzzy/contains distance thresholds. The held item therefore differs from the
// existing one only as a fresh-UUID entry (the component's documented "Add
// anyway keeps it as a separate entry" behaviour), which is exactly what the
// sync path under test enqueues.

import { test, expect } from '@playwright/test';
import {
  SEED,
  openList,
  addItem,
  expectDexieEntry,
  dexieEntries,
  queuedAddCount,
  activeRow,
  uniqueName
} from './fixtures';

/**
 * Submits `text` through the real Add input WITHOUT the no-prompt assertion the
 * shared `addItem` helper makes (that helper waits for the field to clear, which
 * also happens here, but it does not tolerate the warning staying up). The field
 * is cleared on submit whether or not the detector holds the add, so we just
 * fill + click and let the caller assert the held-vs-committed outcome.
 */
async function submitItem(page: import('@playwright/test').Page, text: string): Promise<void> {
  const input = page.getByRole('textbox', { name: 'Add an item' });
  await input.fill(text);
  await page.getByRole('button', { name: 'Add item' }).click();
  // The component clears the field on submit before deciding hold-vs-add, so an
  // empty value is the signal the submit was processed (held or committed).
  await expect(input).toHaveValue('');
}

test.describe('duplicate-warning "Add anyway" enqueue path', () => {
  test('a held near-duplicate enqueues and syncs only after "Add anyway"', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      const item = uniqueName();

      // 1. Seed the original add (fast no-prompt path) and let it land locally,
      //    so it is an ACTIVE entry the detector will check the re-add against.
      await addItem(a, item);
      await expectDexieEntry(a, SEED.listId, item, 'active');
      await expect(activeRow(a, item)).toBeVisible();

      // Re-adding the identical text trips the detector's `exact` branch and the
      // add is HELD behind the inline warning rather than committed.
      await submitItem(a, item);

      // 2a. The inline DuplicateWarning is shown (role="alert", names the
      //     existing item and offers "Add anyway").
      const warning = a.getByRole('alert');
      await expect(warning).toBeVisible();
      await expect(warning).toContainText('already on your list');
      const addAnyway = a.getByRole('button', { name: 'Add anyway (+1)' });
      await expect(addAnyway).toBeVisible();

      // 2b. The add is genuinely HELD: no SECOND optimistic row appeared (the
      //     original is still the only active row for this name) and NO add_item
      //     mutation has been enqueued for it (still exactly the one from step 1).
      await expect(activeRow(a, item)).toHaveCount(1);
      expect(await queuedAddCount(a, SEED.listId, item)).toBe(1);
      // The original add drained, so its queue row is no longer pending/sent; the
      // detector saw the live active entry, not a phantom. (Sanity: B already has it.)
      await expectDexieEntry(b, SEED.listId, item, 'active');

      // 3. "Add anyway" commits a fresh-UUID add_item. The held add is now
      //    enqueued (a SECOND add_item for the same display_name) and drains.
      await addAnyway.click();
      await expect(warning).toHaveCount(0);

      await expect
        .poll(async () => queuedAddCount(a, SEED.listId, item), {
          message: '"Add anyway" did not enqueue a second add_item',
          timeout: 15_000
        })
        .toBe(2);

      // Two distinct active entries with the same display_name now exist locally
      // (each a separate fresh-UUID row, per the component's documented behaviour).
      await expect
        .poll(
          async () => {
            const entries = await dexieEntries(a, SEED.listId);
            return entries.filter((e) => e.display_name === item && e.status === 'active').length;
          },
          { message: 'second active entry never materialised in A', timeout: 15_000 }
        )
        .toBe(2);
      await expect(activeRow(a, item)).toHaveCount(2);

      // It syncs to the second context exactly like any other add: B converges on
      // two active entries with this name, delivered through the real backend.
      await expect
        .poll(
          async () => {
            const entries = await dexieEntries(b, SEED.listId);
            return entries.filter((e) => e.display_name === item && e.status === 'active').length;
          },
          { message: 'second add never synced to B', timeout: 15_000 }
        )
        .toBe(2);
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });

  test('"Cancel" discards the held add and enqueues nothing', async ({ browser }) => {
    const ctx = await browser.newContext();
    try {
      const page = await openList(ctx);

      const item = uniqueName();

      // Seed the original add and let it land, then re-add the same text to trip
      // the detector and hold the add behind the warning.
      await addItem(page, item);
      await expectDexieEntry(page, SEED.listId, item, 'active');
      await submitItem(page, item);

      const warning = page.getByRole('alert');
      await expect(warning).toBeVisible();

      // Cancel dismisses the warning and restores the held text into the input;
      // crucially it must NOT enqueue an add or leave a second optimistic row.
      await page.getByRole('button', { name: 'Cancel' }).click();
      await expect(warning).toHaveCount(0);

      // The typed text is restored for the user (component's `cancel()` behaviour).
      await expect(page.getByRole('textbox', { name: 'Add an item' })).toHaveValue(item);

      // Nothing new was enqueued and no second row exists: still exactly the one
      // add_item and the one active entry from the original add.
      expect(await queuedAddCount(page, SEED.listId, item)).toBe(1);
      await expect(activeRow(page, item)).toHaveCount(1);
      const active = (await dexieEntries(page, SEED.listId)).filter(
        (e) => e.display_name === item && e.status === 'active'
      );
      expect(active).toHaveLength(1);
    } finally {
      await ctx.close();
    }
  });
});
