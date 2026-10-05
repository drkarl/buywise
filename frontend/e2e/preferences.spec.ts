// Per-item household product preferences (issue #102) — end-to-end.
//
// The acceptance case: set "Heinz only" on beans entirely from the app and prove
// the cheaper own-brand product at a store without a Heinz beans product is
// dropped from the plan; then reject the Heinz product and prove beans fall out
// of the totals. Drives the real backend + SPA exactly as a household would.
//
// The beans item, its two products/offers and the resolved list entry are seeded
// by e2e/seed.sh; Tesco sells Heinz beans (€1.20), Aldi an own-brand (€0.85).

import { test, expect } from '@playwright/test';
import { SEED, openList, listPath } from './fixtures';

const BEANS_ITEM_ID = 'e2e-beans';
const PREFERENCES_PATH = `/items/${BEANS_ITEM_ID}/preferences?list=${SEED.listId}`;

test.describe('household product preferences', () => {
  test('switching away from an allowed-brand rule clears the hidden allow-list', async ({ browser }) => {
    const context = await browser.newContext();
    try {
      const page = await openList(context);
      await page.goto(PREFERENCES_PATH);
      await expect(page.getByRole('heading', { name: 'Baked beans' })).toBeVisible();

      await page.getByLabel('Rule', { exact: true }).selectOption('allowed_set');
      await page.getByLabel('Allowed brands', { exact: true }).fill('Heinz');
      await page.getByRole('button', { name: 'Save preferences' }).click();
      await expect(
        page.getByText('Saved. Your next optimisation will use these preferences.')
      ).toBeVisible();

      // The allow-list control disappears for "Any product". Saving that rule
      // must clear its prior hidden state, or the optimiser would keep rejecting Aldi.
      await page.getByLabel('Rule', { exact: true }).selectOption('any');
      await page.getByRole('button', { name: 'Save preferences' }).click();
      await expect(
        page.getByText('Saved. Your next optimisation will use these preferences.')
      ).toBeVisible();
      const saved = await page.evaluate(async (itemId) => {
        const response = await fetch(`/api/items/${itemId}/preferences`);
        return response.json();
      }, BEANS_ITEM_ID);
      expect(saved.default_constraint_policy).toBe('any');
      expect(saved.constraints.allowed_brands).toEqual([]);

      await page.goto(`${listPath}/optimise`);
      const beansLine = page.locator('article.recommended li.line', { hasText: 'Baked beans' });
      await expect(beansLine).toContainText('Aldi');
      await expect(beansLine).toContainText('€0.85');
    } finally {
      await context.close();
    }
  });

  test('Heinz-only beans stay out of stores without Heinz', async ({ browser }) => {
    const context = await browser.newContext();
    try {
      const page = await openList(context);

      // The seeded beans row carries a preferences link.
      await expect(page.getByText('Baked beans').first()).toBeVisible();

      // Before the rule: the cheaper Aldi own-brand is the recommended plan.
      await page.goto(`${listPath}/optimise`);
      await expect(page.getByRole('heading', { name: 'Optimise basket' })).toBeVisible();
      const recommended = page.locator('article.recommended');
      await expect(recommended).toContainText('Aldi');
      await expect(recommended).toContainText('€0.85');

      // Set "Heinz only" through the preferences screen (set entirely from the app).
      await page.goto(PREFERENCES_PATH);
      await expect(page.getByRole('heading', { name: 'Baked beans' })).toBeVisible();
      await page.getByLabel('Rule', { exact: true }).selectOption('brand');
      await page.getByLabel('Brand', { exact: true }).fill('Heinz');
      await page.getByRole('button', { name: 'Save preferences' }).click();
      await expect(
        page.getByText('Saved. Your next optimisation will use these preferences.')
      ).toBeVisible();

      // After the rule: only the Heinz product at Tesco is priced; Aldi is gone.
      await page.goto(`${listPath}/optimise`);
      await expect(page.getByRole('heading', { name: 'Optimise basket' })).toBeVisible();
      await expect(recommended).toContainText('Tesco');
      await expect(recommended).toContainText('€1.20');
      await expect(recommended).not.toContainText('€0.85');
      // Aldi still belongs in the household plan for other items (e.g. milk).
      // Scope the store assertion to the beans line whose preference changed.
      const beansLine = recommended.locator('li.line', { hasText: 'Baked beans' });
      await expect(beansLine).toContainText('Tesco');
      await expect(beansLine).not.toContainText('Aldi');

      // Reject the Heinz product too: beans drop out of every total, and the
      // warning links back to the preferences that caused it.
      await page.goto(PREFERENCES_PATH);
      const heinzRow = page.locator('li.product', { hasText: 'Heinz Baked Beans' });
      await heinzRow.getByRole('button', { name: 'Reject' }).click();
      await expect(heinzRow.getByText('Rejected')).toBeVisible();

      await page.goto(`${listPath}/optimise`);
      await expect(page.getByRole('heading', { name: 'Optimise basket' })).toBeVisible();
      await expect(page.getByText('Items excluded from totals')).toBeVisible();
      await expect(page.getByText('Baked beans').first()).toBeVisible();
    } finally {
      await context.close();
    }
  });
});
