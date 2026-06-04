import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { db } from '$lib/db/dexie';

// `$app/state` is provided by SvelteKit at runtime; mock it so the page can read
// the route id under test without a running kit server.
const LIST = 'list-42';
vi.mock('$app/state', () => ({
	page: { params: { id: 'list-42' } }
}));

import Page from './+page.svelte';

const PRODUCE = 'cat-produce';
const DAIRY = 'cat-dairy';

/** Seeds the cached catalogue + a couple of list entries straight into Dexie. */
async function seed() {
	await db.categories.bulkPut([
		{ id: PRODUCE, name_en: 'Produce', name_es: 'Frutas', icon: 'carrot', default_order: 1 },
		{ id: DAIRY, name_en: 'Dairy', name_es: 'Lácteos', icon: 'milk', default_order: 2 }
	]);
	await db.store_category_orders.bulkPut([
		// Tesco: produce → dairy
		{ id: 'sco-tesco-produce', store: 'tesco', category_id: PRODUCE, display_order: 1 },
		{ id: 'sco-tesco-dairy', store: 'tesco', category_id: DAIRY, display_order: 2 },
		// Aldi: dairy → produce (reversed)
		{ id: 'sco-aldi-dairy', store: 'aldi', category_id: DAIRY, display_order: 1 },
		{ id: 'sco-aldi-produce', store: 'aldi', category_id: PRODUCE, display_order: 2 }
	]);
	await db.list_entries.bulkPut([
		entry('e-apple', 'Apples', PRODUCE),
		entry('e-milk', 'Milk', DAIRY)
	]);
}

function entry(id: string, display_name: string, category_id: string) {
	return {
		id,
		list_id: LIST,
		resolution_status: 'resolved' as const,
		raw_text: display_name,
		display_name,
		quantity: 1,
		status: 'active' as const,
		category_id,
		last_updated_revision: 0,
		updated_at: '2026-01-01T00:00:00Z'
	};
}

beforeEach(async () => {
	await db.delete();
	await db.open();
});

describe('shopping page', () => {
	it('switching store re-orders the category groups', async () => {
		await seed();
		render(Page);

		// Tesco order: Produce before Dairy.
		await waitFor(() => {
			const titles = screen.getAllByText(/Produce|Dairy/).map((n) => n.textContent);
			expect(titles).toEqual(['Produce', 'Dairy']);
		});

		// Switch to Aldi (reversed aisle layout): Dairy before Produce.
		await fireEvent.click(screen.getByRole('button', { name: 'Aldi' }));
		await waitFor(() => {
			const titles = screen.getAllByText(/Produce|Dairy/).map((n) => n.textContent);
			expect(titles).toEqual(['Dairy', 'Produce']);
		});
	});

	it('progress bar updates after checking an item off', async () => {
		await seed();
		render(Page);

		const bar = await screen.findByRole('progressbar');
		// Two active items load from Dexie, none bought yet.
		await waitFor(() => expect(bar.getAttribute('aria-valuemax')).toBe('2'));
		expect(bar.getAttribute('aria-valuenow')).toBe('0');

		// Check off Apples.
		await fireEvent.click(screen.getByRole('button', { name: 'Check off Apples' }));

		// The bar reflects one bought item; the queued mutation is pending.
		await waitFor(() => expect(bar.getAttribute('aria-valuenow')).toBe('1'));
		await waitFor(async () => {
			const mut = (await db.mutation_queue.toArray()).find(
				(m) => m.operation_type === 'mark_bought'
			);
			expect(mut?.status).toBe('pending');
		});
	});

	it('hides a category once all its items are bought', async () => {
		await seed();
		render(Page);

		// Produce starts visible (it has one active item, Apples).
		await screen.findByText('Produce');

		// Check off the only Produce item.
		await fireEvent.click(screen.getByRole('button', { name: 'Check off Apples' }));

		// Produce now has zero remaining active items, so its section disappears
		// while Dairy (still active) stays.
		await waitFor(() => expect(screen.queryByText('Produce')).toBeNull());
		expect(screen.queryByText('Dairy')).not.toBeNull();
	});
});
