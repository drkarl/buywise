import { test, expect } from '@playwright/test';
import { login } from './fixtures';

// End-to-end product matching (issue #108): a resolved item with no product
// match is unpriced and prompts for a match; matching a proposed candidate from
// the app prices it on the next optimisation.
//
// Uses the dedicated `list-e2e-match` fixture (seed.sh) so it does not perturb
// the totals the beans/trip specs assert on `list-e2e`.

const MATCH_LIST = 'list-e2e-match';
const RICE_ITEM = 'e2e-ci-rice';
const PREFERENCES_PATH = `/items/${RICE_ITEM}/preferences?list=${MATCH_LIST}`;

test('matching an unmatched item makes it priced on the next optimisation', async ({ browser }) => {
	const context = await browser.newContext();
	try {
		await login(context);
		const page = await context.newPage();

		// The list itself flags the unmatched staple and links to the match flow.
		await page.goto(`/list/${MATCH_LIST}`);
		await expect(page.getByRole('heading', { name: 'Shopping list' })).toBeVisible();
		await expect(
			page.getByRole('link', { name: 'Match a product to Basmati rice' })
		).toBeVisible();

		// The optimise page also surfaces the unpriced item and links to the flow.
		await page.goto(`/list/${MATCH_LIST}/optimise`);
		const excluded = page.locator('section.unresolved');
		await expect(excluded).toContainText('Basmati rice');
		await expect(excluded).toContainText('no matching product found');
		await excluded.getByRole('link', { name: 'Find a match' }).click();
		await expect(page).toHaveURL(new RegExp(`/items/${RICE_ITEM}/preferences`));

		// Candidates are proposed per store, with the price each decision is based on.
		const suggested = page.locator('section[aria-label="Suggested products"]');
		await expect(suggested).toContainText('Tesco Basmati Rice 1kg');
		await expect(suggested).toContainText('Aldi Basmati Rice 1kg');
		await expect(suggested).toContainText('€2.49');
		await expect(suggested).toContainText('€1.99');

		// Match the Tesco product; it moves into the known-products list.
		const tescoRow = suggested.locator('li.product', { hasText: 'Tesco Basmati Rice 1kg' });
		await tescoRow.getByRole('button', { name: 'Match' }).click();
		await expect(page.locator('section[aria-label="Products"]')).toContainText(
			'Tesco Basmati Rice 1kg'
		);
		await page.goto(`/list/${MATCH_LIST}`);
		await expect(
			page.getByRole('link', { name: 'Preferences for Basmati rice' })
		).toBeVisible();

		// After matching, nothing is left out of the totals and the item is priced.
		await page.goto(`/list/${MATCH_LIST}/optimise`);
		await expect(page.locator('section.unresolved')).toHaveCount(0);
		await expect(page.getByText('Basmati rice').first()).toBeVisible();
		await expect(page.getByText('€2.49').first()).toBeVisible();

		// Undoing the match makes the prompt return on the linked list.
		await page.goto(PREFERENCES_PATH);
		const heinz = page.locator('section[aria-label="Products"] li.product', {
			hasText: 'Tesco Basmati Rice 1kg'
		});
		await heinz.getByRole('button', { name: 'Undo' }).click();
		await expect(page.locator('section[aria-label="Suggested products"]')).toContainText(
			'Tesco Basmati Rice 1kg'
		);
		await page.goto(`/list/${MATCH_LIST}`);
		await expect(
			page.getByRole('link', { name: 'Match a product to Basmati rice' })
		).toBeVisible();
	} finally {
		await context.close();
	}
});
