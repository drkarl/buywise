import { test, expect } from '@playwright/test';
import { login, waitForDrained } from './fixtures';

// End-to-end purchase suggestions (issue #106): weekly check-off history makes an
// item due and it appears in a separate "Suggested" section; confirming adds it
// to the list and dismissing removes it for one typical interval. Uses the
// dedicated `list-e2e-suggest` fixture (seed.sh).

const SUGGEST_LIST = 'list-e2e-suggest';
const LIST_PATH = `/list/${SUGGEST_LIST}`;

test('history leads to a suggestion; confirm adds the item and dismiss removes it', async ({
	browser
}) => {
	const context = await browser.newContext();
	try {
		await login(context);
		const page = await context.newPage();

		await page.goto(LIST_PATH);
		await expect(page.getByRole('heading', { name: 'Shopping list' })).toBeVisible();

		// Both weekly staples are due and appear in the Suggested section.
		const suggested = page.locator('section[aria-label="Suggested"]');
		await expect(suggested).toContainText('Oat milk');
		await expect(suggested).toContainText('Soy sauce');

		// Confirming adds the item to the list and removes its suggestion.
		await page.getByRole('button', { name: 'Add Oat milk' }).click();
		await expect(page.getByRole('group', { name: 'Oat milk', exact: true })).toBeVisible();
		await expect(suggested).not.toContainText('Oat milk');

		// Dismissing removes the other suggestion without adding it. The section
		// disappears entirely once no suggestions remain.
		await page.getByRole('button', { name: 'Dismiss Soy sauce' }).click();
		await expect(page.getByRole('button', { name: 'Dismiss Soy sauce' })).toHaveCount(0);
		await expect(page.getByRole('group', { name: 'Soy sauce', exact: true })).toHaveCount(0);
		await expect(suggested).toHaveCount(0);

		await waitForDrained(page, SUGGEST_LIST);

		// Reloading proves both effects persisted on the server: Oat milk is now an
		// active entry (excluded), and Soy sauce stays dismissed for one interval.
		await page.reload();
		await expect(page.getByRole('group', { name: 'Oat milk', exact: true })).toBeVisible();
		await expect(page.locator('section[aria-label="Suggested"]')).toHaveCount(0);
	} finally {
		await context.close();
	}
});
