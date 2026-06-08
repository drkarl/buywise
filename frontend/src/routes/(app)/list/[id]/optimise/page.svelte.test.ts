import { afterEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';
import type { OptimiseResponse } from '$lib/api/optimise';

// `$app/state` is provided by SvelteKit at runtime; mock it so the page can read
// the route id under test without a running kit server.
vi.mock('$app/state', () => ({
	page: { params: { id: 'list-42' } }
}));

import Page from './+page.svelte';

/** A response exercising recommended + all four plans + both excluded buckets. */
function fullResponse(): OptimiseResponse {
	return {
		run_id: 'run-1',
		best_single: {
			stores: ['tesco'],
			lines: [
				{
					item_index: 0,
					display_text: 'Milk 2L',
					retailer: 'tesco',
					retailer_product_id: 'rp-1',
					offer_id: 'of-1',
					unit_price_cents: 300,
					line_total_cents: 300,
					loyalty_applied: false,
					freshness_confidence: 'high'
				}
			],
			total_cents: 500,
			missing_count: 0,
			dunnes_voucher_offer_text: null
		},
		best_two_store: {
			stores: ['tesco', 'aldi'],
			lines: [
				{
					item_index: 0,
					display_text: 'Milk 2L',
					retailer: 'aldi',
					retailer_product_id: 'rp-9',
					offer_id: 'of-9',
					unit_price_cents: 250,
					line_total_cents: 250,
					loyalty_applied: false,
					freshness_confidence: 'unknown'
				}
			],
			total_cents: 400,
			missing_count: 0,
			dunnes_voucher_offer_text: null
		},
		theoretical_minimum: {
			stores: ['tesco', 'aldi', 'lidl'],
			lines: [],
			total_cents: 380,
			missing_count: 0,
			dunnes_voucher_offer_text: null
		},
		recommended: {
			kind: 'two_store',
			clears_convenience_threshold: true,
			two_store_saving_cents: 100,
			plan: {
				stores: ['tesco', 'aldi'],
				lines: [],
				total_cents: 400,
				missing_count: 0,
				dunnes_voucher_offer_text: null
			}
		},
		unresolved_items: [{ item_index: 2, display_text: 'cream' }],
		no_valid_offer_items: [{ item_index: 3, display_text: 'truffle oil', reason: 'no_matches' }],
		plain_language_explanation: 'Splitting between Tesco and Aldi saves you the most.',
		offer_snapshot_date: '2026-06-07'
	};
}

function stubOptimise(response: OptimiseResponse, status = 200) {
	const fetchMock = vi.fn(async (input: RequestInfo | URL, _init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		if (url.includes('/api/optimise')) {
			return new Response(JSON.stringify(response), { status });
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

describe('optimise page', () => {
	it('POSTs to /api/optimise on mount with the list id', async () => {
		const fetchMock = stubOptimise(fullResponse());
		render(Page);

		await waitFor(() => expect(fetchMock).toHaveBeenCalled());
		const [, init] = fetchMock.mock.calls[0];
		expect(init?.method).toBe('POST');
		expect(JSON.parse(init?.body as string)).toEqual({ list_id: 'list-42' });
	});

	it('shows a loading state before results arrive', () => {
		// Never-resolving fetch keeps the page in its loading state.
		vi.stubGlobal('fetch', vi.fn(() => new Promise(() => {})));
		render(Page);
		expect(screen.getByRole('status').textContent).toMatch(/costing/i);
	});

	it('renders the recommended plan as visually distinct', async () => {
		stubOptimise(fullResponse());
		render(Page);
		// The recommended card exposes an accessible name flagging it as recommended.
		await waitFor(() => expect(screen.getByLabelText(/recommended/i)).toBeTruthy());
	});

	it('renders the plain-language explanation', async () => {
		stubOptimise(fullResponse());
		render(Page);
		await screen.findByText(/Splitting between Tesco and Aldi/);
	});

	it('lists unresolved items separately with a clarify link to the list view', async () => {
		stubOptimise(fullResponse());
		render(Page);

		// The unresolved item is named and linked back to the list's clarify flow.
		await screen.findByText(/cream — needs clarifying/);
		const clarify = screen.getByRole('link', { name: /clarify on your list/i });
		expect(clarify.getAttribute('href')).toBe('/list/list-42');

		// The no-valid-offer item appears with its reason in the same section.
		expect(screen.getByText(/truffle oil — no matching product found/)).toBeTruthy();
	});

	it('surfaces a visible error on a non-OK response', async () => {
		stubOptimise(fullResponse(), 422);
		render(Page);

		const alert = await screen.findByRole('alert');
		expect(alert.textContent).toMatch(/422/);
		// And offers a retry affordance.
		expect(screen.getByRole('button', { name: /try again/i })).toBeTruthy();
	});

	it('renders an empty-state when every plan is null', async () => {
		const empty: OptimiseResponse = {
			run_id: 'run-empty',
			best_single: null,
			best_two_store: null,
			theoretical_minimum: null,
			recommended: null,
			unresolved_items: [],
			no_valid_offer_items: [],
			plain_language_explanation: '',
			offer_snapshot_date: null
		};
		stubOptimise(empty);
		render(Page);
		await screen.findByText(/no plans to show/i);
	});
});
