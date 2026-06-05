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

beforeEach(async () => {
	await db.delete();
	await db.open();
});

describe('list page', () => {
	it('adding an item queues a pending add_item mutation and shows it as active', async () => {
		render(Page);

		const input = screen.getByLabelText('Add an item');
		await fireEvent.input(input, { target: { value: 'Olive oil' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Add item' }));

		// The optimistic entry renders.
		await screen.findByText('Olive oil');

		// A pending add_item mutation was queued in Dexie immediately.
		await waitFor(async () => {
			const muts = await db.mutation_queue.toArray();
			expect(muts).toHaveLength(1);
			expect(muts[0]).toMatchObject({
				operation_type: 'add_item',
				status: 'pending',
				list_id: LIST
			});
		});

		// Optimistic active entry exists in Dexie.
		const entries = await db.list_entries.where('list_id').equals(LIST).toArray();
		expect(entries).toHaveLength(1);
		expect(entries[0].status).toBe('active');
	});

	it('header active-count is reactive and bought items collapse into "In basket"', async () => {
		render(Page);
		const input = screen.getByLabelText('Add an item');

		await fireEvent.input(input, { target: { value: 'Bread' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Add item' }));
		await screen.findByText('Bread');

		// Header reflects 1 active item.
		await screen.findByText('1 item');

		// Check it off.
		await fireEvent.click(screen.getByRole('button', { name: 'Check off Bread' }));

		// Local record flips to bought immediately.
		await waitFor(async () => {
			const [entry] = await db.list_entries.toArray();
			expect(entry.status).toBe('bought');
		});

		// mark_bought mutation queued.
		const boughtMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'mark_bought'
		);
		expect(boughtMut?.status).toBe('pending');

		// Header now shows 0 active items, and the basket section appears.
		await screen.findByText('0 items');
		await screen.findByRole('button', { name: /In basket \(1\)/ });
	});

	it('deleting an item removes its row from the active list', async () => {
		render(Page);
		const input = screen.getByLabelText('Add an item');
		await fireEvent.input(input, { target: { value: 'Mustard' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Add item' }));
		await screen.findByText('Mustard');

		// Open the long-press action menu on the row, then hit Delete.
		const row = screen.getByRole('group', { name: 'Mustard' });
		vi.useFakeTimers();
		await fireEvent.pointerDown(row, { clientX: 0, pointerId: 1 });
		vi.advanceTimersByTime(500); // long-press threshold
		vi.useRealTimers();

		await fireEvent.click(await screen.findByRole('button', { name: 'Delete' }));

		// Row is gone from the UI and the entry is soft-deleted in Dexie.
		await waitFor(() => expect(screen.queryByText('Mustard')).toBeNull());
		const [entry] = await db.list_entries.toArray();
		expect(entry.status).toBe('deleted');

		// A delete_item mutation was queued.
		const delMut = (await db.mutation_queue.toArray()).find(
			(m) => m.operation_type === 'delete_item'
		);
		expect(delMut?.status).toBe('pending');
	});

	it('surfaces a visible alert when the server rejects a queued mutation', async () => {
		// Stub the global fetch the sync engine uses: reject the mutation POST with a
		// terminal 4xx, and serve an active-only snapshot (so reconcile removes the
		// phantom). The onFailed wiring should then render an accessible alert.
		const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.includes('/api/list/')) {
				return new Response(
					JSON.stringify({ id: LIST, name: 'L', current_revision: 0, entries: [] }),
					{ status: 200 }
				);
			}
			return new Response('rejected by server', { status: 422 });
		});
		vi.stubGlobal('fetch', fetchMock);

		try {
			render(Page);
			const input = screen.getByLabelText('Add an item');
			await fireEvent.input(input, { target: { value: 'Caviar' } });
			await fireEvent.click(screen.getByRole('button', { name: 'Add item' }));
			await screen.findByText('Caviar');

			// The enqueue signals the running engine, which drains, gets the 422,
			// and calls onFailed → an alert appears mentioning the operation + reason.
			const alert = await screen.findByRole('alert');
			expect(alert.textContent).toMatch(/add/i);
			expect(alert.textContent).toMatch(/rejected by server/);

			// Dismissible.
			await fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
			await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
		} finally {
			vi.unstubAllGlobals();
		}
	});

	describe('ambiguity confirmation flow (P3-T3)', () => {
		/** Seed an ambiguous entry + its candidate catalogue names, as sync would. */
		async function seedAmbiguous() {
			await db.list_entries.add({
				id: 'amb-1',
				list_id: LIST,
				resolution_status: 'ambiguous',
				resolution_candidates_json: JSON.stringify(['ci-single', 'ci-double']),
				raw_text: 'cream',
				display_name: 'cream',
				quantity: 1,
				status: 'active',
				last_updated_revision: 1,
				updated_at: '2026-06-05T00:00:00.000Z'
			});
			await db.item_categories.bulkPut([
				{ canonical_item_id: 'ci-single', category_id: 'dairy', name_en: 'Single cream' },
				{ canonical_item_id: 'ci-double', category_id: 'dairy', name_en: 'Double cream' }
			]);
		}

		it('shows the "? Tap to clarify" indicator and opens a modal with the candidate labels', async () => {
			await seedAmbiguous();
			render(Page);

			const clarify = await screen.findByRole('button', { name: 'Clarify cream' });
			expect(clarify.textContent).toMatch(/clarify/i);

			await fireEvent.click(clarify);

			// Modal lists the candidate items by their cached catalogue names.
			await screen.findByRole('dialog');
			expect(screen.getByText('Single cream')).toBeTruthy();
			expect(screen.getByText('Double cream')).toBeTruthy();
		});

		it('confirming a candidate resolves the entry and queues a resolve_alias mutation', async () => {
			await seedAmbiguous();
			render(Page);

			await fireEvent.click(await screen.findByRole('button', { name: 'Clarify cream' }));
			await screen.findByRole('dialog');
			await fireEvent.click(screen.getByRole('radio', { name: 'Double cream' }));
			await fireEvent.click(screen.getByRole('button', { name: 'Confirm' }));

			// Entry flips to resolved with the chosen item; the modal closes.
			await waitFor(async () => {
				const entry = await db.list_entries.get('amb-1');
				expect(entry?.resolution_status).toBe('resolved');
				expect(entry?.canonical_item_id).toBe('ci-double');
			});
			await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

			const muts = await db.mutation_queue.toArray();
			const resolveMut = muts.find((m) => m.operation_type === 'resolve_alias');
			expect(resolveMut).toMatchObject({
				entity_id: 'amb-1',
				status: 'pending',
				payload: { chosen_canonical_item_id: 'ci-double', raw_text: 'cream' }
			});
		});

		it('"Keep as-is" closes the modal and leaves the entry ambiguous (no mutation)', async () => {
			await seedAmbiguous();
			render(Page);

			await fireEvent.click(await screen.findByRole('button', { name: 'Clarify cream' }));
			await screen.findByRole('dialog');
			await fireEvent.click(screen.getByRole('button', { name: 'Keep as-is' }));

			await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());

			const entry = await db.list_entries.get('amb-1');
			expect(entry?.resolution_status).toBe('ambiguous');
			const resolveMut = (await db.mutation_queue.toArray()).find(
				(m) => m.operation_type === 'resolve_alias'
			);
			expect(resolveMut).toBeUndefined();
		});
	});

	it('bought and skipped items both group under "In basket"', async () => {
		render(Page);
		const input = screen.getByLabelText('Add an item');

		await fireEvent.input(input, { target: { value: 'Rice' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Add item' }));
		await screen.findByText('Rice');

		await fireEvent.input(input, { target: { value: 'Salt' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Add item' }));
		await screen.findByText('Salt');

		// Check off one, skip the other.
		await fireEvent.click(screen.getByRole('button', { name: 'Check off Rice' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Skip Salt' }));

		// Both land in the basket section.
		await screen.findByText('0 items');
		await screen.findByRole('button', { name: /In basket \(2\)/ });

		// Expand the basket and verify both rows are present.
		await fireEvent.click(screen.getByRole('button', { name: /In basket \(2\)/ }));
		await screen.findByText('Rice');
		await screen.findByText('Salt');
	});
});
