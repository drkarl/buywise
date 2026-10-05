import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { db } from '$lib/db/dexie';
import { saveChosenTrip, type ChosenTrip } from '$lib/shopping/chosenTrip';

// `$app/state` is provided by SvelteKit at runtime; mock it so the page can read
// the route id under test without a running kit server.
const LIST = 'list-42';
vi.mock('$app/state', () => ({
	page: { params: { id: 'list-42' } }
}));

import Page from './+page.svelte';

const PRODUCE = 'cat-produce';
const DAIRY = 'cat-dairy';

function entry(id: string, display_name: string, category_id: string, quantity = 1) {
	return {
		id,
		list_id: LIST,
		resolution_status: 'resolved' as const,
		raw_text: display_name,
		display_name,
		quantity,
		status: 'active' as const,
		category_id,
		last_updated_revision: 0,
		updated_at: '2026-01-01T00:00:00Z'
	};
}

/** Seeds the cached catalogue + entries, and a chosen two-store trip. */
async function seed() {
	await db.categories.bulkPut([
		{ id: PRODUCE, name_en: 'Produce', name_es: 'Frutas', icon: 'carrot', default_order: 1 },
		{ id: DAIRY, name_en: 'Dairy', name_es: 'Lácteos', icon: 'milk', default_order: 2 }
	]);
	await db.store_category_orders.bulkPut([
		{ id: 'sco-tesco-produce', store: 'tesco', category_id: PRODUCE, display_order: 1 },
		{ id: 'sco-tesco-dairy', store: 'tesco', category_id: DAIRY, display_order: 2 },
		{ id: 'sco-aldi-dairy', store: 'aldi', category_id: DAIRY, display_order: 1 },
		{ id: 'sco-aldi-produce', store: 'aldi', category_id: PRODUCE, display_order: 2 }
	]);
	await db.list_entries.bulkPut([
		entry('e-apple', 'Apples', PRODUCE, 2),
		entry('e-milk', 'Milk', DAIRY),
		entry('e-cream', 'Cream', DAIRY)
	]);

	const trip: ChosenTrip = {
		listId: LIST,
		runId: 'run-1',
		planKind: 'two_store',
		title: 'Recommended split',
		savedAt: '2026-01-01T00:00:00Z',
		allocation: {
			stores: ['tesco', 'aldi'],
			byStore: [
				{
					retailer: 'tesco',
					lines: [
						{
							entryId: 'e-milk',
							retailer: 'tesco',
							retailerProductId: 'rp-milk',
							productName: 'Tesco Whole Milk 2L',
							unitPriceCents: 120,
							lineTotalCents: 120,
							loyaltyApplied: false,
							freshnessConfidence: 'high'
						}
					]
				},
				{
					retailer: 'aldi',
					lines: [
						{
							entryId: 'e-apple',
							retailer: 'aldi',
							retailerProductId: 'rp-apple',
							productName: 'Aldi Gala Apples 6 Pack',
							unitPriceCents: 90,
							lineTotalCents: 180,
							loyaltyApplied: false,
							freshnessConfidence: 'high'
						}
					]
				}
			],
			unallocated: [{ entryId: 'e-cream', reason: 'unresolved' }],
			entryIds: ['e-milk', 'e-apple', 'e-cream']
		}
	};
	saveChosenTrip(trip);
}

beforeEach(async () => {
	await db.delete();
	await db.open();
	localStorage.clear();
});

describe('trip page', () => {
	it('renders one list per store plus the explicit unallocated section', async () => {
		await seed();
		render(Page);

		// The sections render immediately; their live-query contents fill in after
		// Dexie resolves, so wait for the first store's item to appear.
		await waitFor(() => {
			const tesco = screen.getByRole('region', { name: 'Tesco trip' });
			expect(tesco.textContent).toMatch(/Milk/);
		});

		const tesco = screen.getByRole('region', { name: 'Tesco trip' });
		const aldi = screen.getByRole('region', { name: 'Aldi trip' });
		// Each store's own items only — no cross-store leakage.
		expect(tesco.textContent).not.toMatch(/Apples/);
		expect(aldi.textContent).not.toMatch(/Milk/);

		// Chosen product label and quantity are shown on the allocated lines.
		expect(tesco.textContent).toMatch(/Tesco Whole Milk 2L/);
		expect(aldi.textContent).toMatch(/Aldi Gala Apples 6 Pack/);
		expect(aldi.textContent).toMatch(/×2/);

		// The unallocated item is kept in its explicit section, with the reason.
		const unallocated = screen.getByRole('region', { name: 'Not allocated to a store' });
		expect(unallocated.textContent).toMatch(/Cream/);
		expect(unallocated.textContent).toMatch(/needs clarifying/);
	});

	it('checks an item off through the shared offline queue', async () => {
		await seed();
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Check off Milk' }));

		// The one shared entry is optimistically bought and a queued mutation waits
		// for the sync engine (same path as the main list and shopping views).
		await waitFor(async () => {
			const stored = await db.list_entries.get('e-milk');
			expect(stored?.status).toBe('bought');
			const mut = (await db.mutation_queue.toArray()).find(
				(m) => m.entity_id === 'e-milk' && m.operation_type === 'mark_bought'
			);
			expect(mut?.status).toBe('pending');
		});
	});

	it('warns when the list changed since the plan was chosen', async () => {
		await seed();
		// An entry the plan does not cover makes the cached plan stale.
		await db.list_entries.put(entry('e-bread', 'Bread', PRODUCE));
		render(Page);

		const banner = await screen.findByRole('alert');
		expect(banner.textContent).toMatch(/list has changed/i);
	});

	it('offers the optimiser when no plan has been chosen', async () => {
		render(Page);
		expect(await screen.findByText(/No plan selected yet/i)).toBeTruthy();
	});
});
