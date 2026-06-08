import { afterEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/svelte';
import type { SavingsSummary } from '$lib/api/savings';

import Page from './+page.svelte';

/** A summary exercising two months, a cumulative figure, and both list states. */
function fullSummary(): SavingsSummary {
	return {
		monthly: [
			{ month: '2026-05', gross_cents: 4000, accepted_cents: 3700, net_cents: 3300, lists: 2 },
			{ month: '2026-06', gross_cents: 2000, accepted_cents: 0, net_cents: 0, lists: 1 }
		],
		cumulative_net_cents: 3300,
		lists: [
			{
				id: 'ev-3',
				list_id: 'list-3',
				list_name: 'June shop',
				created_at: '2026-06-01T10:00:00Z',
				gross_saving_cents: 2000,
				chosen_basket_cost_cents: null,
				net_saving_cents: null
			},
			{
				id: 'ev-1',
				list_id: 'list-1',
				list_name: 'May shop',
				created_at: '2026-05-03T10:00:00Z',
				gross_saving_cents: 3000,
				chosen_basket_cost_cents: 4500,
				net_saving_cents: 2500
			}
		]
	};
}

/** Stub global fetch to answer `/api/savings/summary` with `summary`/`status`. */
function stubSummary(summary: SavingsSummary, status = 200) {
	const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
		const url = typeof input === 'string' ? input : input.toString();
		if (url.includes('/api/savings/summary')) {
			return new Response(JSON.stringify(summary), { status });
		}
		return new Response('not found', { status: 404 });
	});
	vi.stubGlobal('fetch', fetchMock);
	return fetchMock;
}

afterEach(() => {
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
});

describe('savings dashboard', () => {
	it('renders the cumulative net saving', async () => {
		stubSummary(fullSummary());
		render(Page);
		const region = await screen.findByRole('region', { name: 'Cumulative saving' });
		// €33.00 (3300 cents) also appears as May's net; scope to the cumulative
		// section so we assert the headline figure specifically.
		expect(within(region).getByText('€33.00')).toBeTruthy();
	});

	it('renders a month card per monthly entry with its figures', async () => {
		stubSummary(fullSummary());
		render(Page);
		await waitFor(() => expect(screen.getByText('May 2026')).toBeTruthy());
		expect(screen.getByText('June 2026')).toBeTruthy();
		// May gross 4000 cents → €40.00; the per-month list counts are rendered.
		expect(screen.getByText('€40.00')).toBeTruthy();
		expect(screen.getByText('2 lists')).toBeTruthy();
		expect(screen.getByText('1 list')).toBeTruthy();
	});

	it('shows an "Enter actual spend" link only for lists with a null chosen cost', async () => {
		stubSummary(fullSummary());
		render(Page);
		await waitFor(() => expect(screen.getByText('June shop')).toBeTruthy());
		// The un-reconciled June list gets the prompt, linking to its list view.
		const link = screen.getByRole('link', { name: 'Enter actual spend' });
		expect(link.getAttribute('href')).toBe('/list/list-3');
		// The reconciled May list shows its net saving instead of the prompt.
		expect(screen.getByText('Net €25.00')).toBeTruthy();
		// Exactly one prompt (only the null-chosen list).
		expect(screen.getAllByRole('link', { name: 'Enter actual spend' })).toHaveLength(1);
	});

	it('renders empty-state copy when there are no savings', async () => {
		stubSummary({ monthly: [], cumulative_net_cents: 0, lists: [] });
		render(Page);
		const region = await screen.findByRole('region', { name: 'Cumulative saving' });
		expect(within(region).getByText('€0.00')).toBeTruthy();
		expect(
			screen.getByText('No savings yet — archive a list with a recommendation to start tracking.')
		).toBeTruthy();
	});

	it('shows an error with a retry on a non-OK response', async () => {
		stubSummary(fullSummary(), 500);
		render(Page);
		const alert = await screen.findByRole('alert');
		expect(alert.textContent).toMatch(/couldn’t load your savings/i);
		expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
	});
});
