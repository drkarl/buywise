import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';
import { db } from '$lib/db/dexie';
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

// The page now flushes the pending mutation queue before pricing, which reads
// Dexie. Reset the fake-indexeddb between tests so a seeded `pending` mutation
// from one test can't leak into another (mirrors SyncQueue.test.ts).
beforeEach(async () => {
	await db.delete();
	await db.open();
});

afterEach(() => {
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
});

describe('optimise page', () => {
	/** The init of the first `POST /api/optimise` call recorded by the mock. */
	function optimiseCallInit(fetchMock: ReturnType<typeof stubOptimise>): RequestInit | undefined {
		const call = fetchMock.mock.calls.find(([input]) =>
			(typeof input === 'string' ? input : input.toString()).includes('/api/optimise')
		);
		return call?.[1];
	}

	it('POSTs to /api/optimise on mount with the list id', async () => {
		const fetchMock = stubOptimise(fullResponse());
		render(Page);

		// With an empty queue the flush is a no-op, so the optimise POST fires.
		await waitFor(() => expect(optimiseCallInit(fetchMock)).toBeDefined());
		const init = optimiseCallInit(fetchMock);
		expect(init?.method).toBe('POST');
		expect(JSON.parse(init?.body as string)).toEqual({ list_id: 'list-42' });
	});

	it('carries the CSRF header on the POST when a token cookie is present', async () => {
		// The backend rejects mutating calls without X-CSRF-Token; the page builds
		// headers via jsonMutationHeaders(), which reads the `csrf` cookie.
		document.cookie = 'csrf=tok-abc123';
		try {
			const fetchMock = stubOptimise(fullResponse());
			render(Page);

			await waitFor(() => expect(optimiseCallInit(fetchMock)).toBeDefined());
			const headers = optimiseCallInit(fetchMock)?.headers as Record<string, string>;
			expect(headers['X-CSRF-Token']).toBe('tok-abc123');
		} finally {
			// Don't leak the cookie into other tests.
			document.cookie = 'csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT';
		}
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

	it('lists unresolved items separately with a review link to the list view', async () => {
		stubOptimise(fullResponse());
		render(Page);

		// The unresolved item is named and linked back to the list view. The link
		// says "Review" (not "Clarify") because the response carries no
		// resolution_status, so we can't promise a clarify control for a row that
		// may be plain `unresolved` rather than `ambiguous`.
		await screen.findByText(/cream — needs clarifying/);
		const review = screen.getAllByRole('link', { name: /review on your list/i })[0];
		expect(review.getAttribute('href')).toBe('/list/list-42');

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

	it('suppresses the saving line on other plans when best_single is null', async () => {
		// No single-store baseline exists, so no card can show a "saving vs best
		// single store" line — there is nothing to compare against.
		const noBaseline = fullResponse();
		noBaseline.best_single = null;
		stubOptimise(noBaseline);
		render(Page);

		// The two-store and recommended plans still render…
		await screen.findByText('Cheapest two-store split');
		// …but with no baseline, no saving/comparison line appears anywhere.
		expect(screen.queryByText(/best single store/i)).toBeNull();
		expect(screen.queryByText(/Saves/)).toBeNull();
	});

	/** Seeds one `pending` mutation for list-42, mimicking an offline-first edit. */
	async function seedPendingMutation() {
		await db.mutation_queue.add({
			client_mutation_id: 'cmid-pending-1',
			device_id: 'dev-1',
			list_id: 'list-42',
			entity_type: 'list_entry',
			entity_id: 'entry-1',
			operation_type: 'add_item',
			payload: {},
			base_revision: 0,
			created_at_client: '2026-06-07T00:00:00.000Z',
			status: 'pending',
			retry_count: 0
		});
	}

	it('flushes pending edits before optimising and prices once they confirm', async () => {
		await seedPendingMutation();

		// Record call order so we can assert the drain ran before pricing.
		const seen: string[] = [];
		const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.includes('/api/mutations')) {
				seen.push('mutations');
				const sent = JSON.parse(String(init?.body));
				return new Response(
					JSON.stringify({
						client_mutation_id: sent.client_mutation_id,
						status: 'accepted',
						server_revision: 1,
						entity: {
							id: sent.entity_id,
							list_id: 'list-42',
							canonical_item_id: null,
							resolution_status: 'unresolved',
							raw_text: 'Milk',
							display_name: 'Milk',
							quantity: 1,
							status: 'active',
							bought_by: null,
							bought_at: null,
							category_id: null,
							last_updated_revision: 1,
							updated_at: '2026-06-07T00:01:00.000Z'
						}
					}),
					{ status: 200 }
				);
			}
			if (url.includes('/api/optimise')) {
				seen.push('optimise');
				return new Response(JSON.stringify(fullResponse()), { status: 200 });
			}
			return new Response('not found', { status: 404 });
		});
		vi.stubGlobal('fetch', fetchMock);
		render(Page);

		// The plan renders, proving the pending edit was confirmed first.
		await screen.findByText(/Splitting between Tesco and Aldi/);
		// And the drain POST preceded the optimise POST.
		expect(seen.indexOf('mutations')).toBeGreaterThanOrEqual(0);
		expect(seen.indexOf('mutations')).toBeLessThan(seen.indexOf('optimise'));
	});

	it('blocks optimising when a pending edit cannot be confirmed (offline)', async () => {
		await seedPendingMutation();

		// The mutation POST keeps throwing (transport error → stays `pending`); the
		// optimise endpoint must never be reached.
		const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.includes('/api/mutations')) {
				throw new TypeError('Failed to fetch'); // offline
			}
			if (url.includes('/api/optimise')) {
				return new Response(JSON.stringify(fullResponse()), { status: 200 });
			}
			return new Response('not found', { status: 404 });
		});
		vi.stubGlobal('fetch', fetchMock);
		render(Page);

		// The page surfaces the unsynced-edits error… The drain exhausts its
		// transport retries (MAX_RETRIES with 500/1000/2000ms backoff) before
		// reporting `synced: false`, so allow a generous timeout for the real
		// backoff to elapse (the page doesn't expose `sleep` for injection).
		const alert = await screen.findByRole('alert', {}, { timeout: 6000 });
		expect(alert.textContent).toMatch(/haven’t been saved to the server/i);
		// …and never asked the optimiser to price the stale list.
		const optimiseCalls = fetchMock.mock.calls.filter(([input]) =>
			(typeof input === 'string' ? input : input.toString()).includes('/api/optimise')
		);
		expect(optimiseCalls).toHaveLength(0);
	});
});
