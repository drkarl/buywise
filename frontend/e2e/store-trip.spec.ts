// Per-store trip view of the chosen plan (issue #104).
//
// Choosing the two-store plan on /optimise must produce two per-store lists plus
// the explicit unallocated section, together accounting for the whole seeded
// list; and a check-off made in a trip while OFFLINE must flow through the normal
// offline queue and converge across devices on reconnect. The seed (e2e/seed.sh)
// makes Milk cheapest at Aldi and Bread cheapest at Tesco (both seeded RESOLVED),
// with one unresolved entry, so the optimiser's best two-store split is real and
// deterministic.

import { test, expect } from '@playwright/test';
import {
	SEED,
	openList,
	login,
	expectDexieEntry,
	serverEntries,
	waitForDrained
} from './fixtures';

test.describe('per-store trip view', () => {
	test('two-store plan gives two lists plus unallocated, and an offline check-off converges', async ({
		browser
	}) => {
		const ctxA = await browser.newContext();
		const ctxB = await browser.newContext();
		try {
			// A: choose the plan. B: an online peer to observe convergence via SSE.
			await login(ctxA);
			const a = await ctxA.newPage();
			const b = await openList(ctxB);

			await a.goto(`/list/${SEED.listId}/optimise`);

			// The optimiser returns a real two-store split for the seeded basket.
			const twoStoreCard = a
				.getByRole('article')
				.filter({ hasText: 'Cheapest two-store split' });
			await expect(twoStoreCard).toBeVisible();

			await twoStoreCard.getByRole('link', { name: 'Shop this plan' }).click();
			await expect(a).toHaveURL(new RegExp(`/list/${SEED.listId}/trip$`));

			// Two separate store lists, each grouped by category.
			const storeSections = a.locator('section.store-trip');
			await expect(storeSections).toHaveCount(2);
			const storeText = (await storeSections.allTextContents()).join('\n');
			// The two priced items are allocated to their own store lists, and the
			// quantity is carried once (Milk was seeded with quantity 2).
			expect(storeText).toMatch(/Milk/);
			expect(storeText).toMatch(/Bread/);
			expect(storeText).toMatch(/×2/);

			// The unresolved entry is kept in the explicit unallocated section, so the
			// two lists plus that section account for the original list.
			const unallocated = a.getByRole('region', { name: 'Not allocated to a store' });
			await expect(unallocated).toContainText('Mystery sauce');
			expect(storeText).not.toMatch(/Mystery sauce/);

			// A goes offline and checks Milk off in its trip list.
			await ctxA.setOffline(true);
			await a.getByRole('button', { name: 'Check off Milk' }).click();
			// The optimistic local row is bought while the mutation waits, offline.
			await expectDexieEntry(a, SEED.listId, 'Milk', 'bought');

			// Reconnect → the queue drains and both devices converge.
			await ctxA.setOffline(false);
			await waitForDrained(a, SEED.listId);

			await expectDexieEntry(b, SEED.listId, 'Milk', 'bought');
			await expectDexieEntry(a, SEED.listId, 'Milk', 'bought');

			const server = (await serverEntries(ctxB, SEED.listId)).find(
				(e) => e.display_name === 'Milk'
			);
			expect(server?.status).toBe('bought');
			// The check-off updated the SHARED entry's status only — quantity is
			// untouched, so it was never double-counted by the trip view.
			expect(server?.quantity).toBe(2);
		} finally {
			await ctxA.close();
			await ctxB.close();
		}
	});
});
