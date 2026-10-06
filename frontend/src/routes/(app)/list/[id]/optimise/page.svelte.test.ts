import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { db } from '$lib/db/dexie';
import type { OptimiseResponse } from '$lib/api/optimise';
import type { Settings } from '$lib/api/settings';
import type { ChosenTrip } from '$lib/shopping/chosenTrip';

// `$app/state` is provided by SvelteKit at runtime; mock it so the page can read
// the route id under test without a running kit server.
vi.mock('$app/state', () => ({
	page: { params: { id: 'list-42' } }
}));

import Page from './+page.svelte';

/** A stubbed `fetch` (as installed by {@link stubOptimise}). */
type FetchMock = Mock<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>;

/** A response exercising recommended + all four plans + both excluded buckets. */
function fullResponse(): OptimiseResponse {
	return {
		basket_items: [
			{ item_index: 0, entry_id: 'e-0', display_text: 'Milk 2L' },
			{ item_index: 1, entry_id: 'e-1', display_text: 'bread' },
			{ item_index: 2, entry_id: 'e-2', display_text: 'cream' },
			{ item_index: 3, entry_id: 'e-3', display_text: 'truffle oil' }
		],
		run_id: 'run-1',
		best_single: {
			stores: ['tesco'],
			lines: [
				{
					item_index: 0,
					entry_id: 'e-0',
					product_name: 'Product 0',
					display_text: 'Milk 2L',
					canonical_item_id: 'ci-1',
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
					entry_id: 'e-0',
					product_name: 'Product 0',
					display_text: 'Milk 2L',
					canonical_item_id: 'ci-1',
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
				lines: [
					{
						item_index: 0,
						entry_id: 'e-0',
						display_text: 'Milk 2L',
						product_name: 'Product 0',
						canonical_item_id: 'ci-0',
						retailer: 'tesco',
						retailer_product_id: 'rp-0',
						offer_id: 'of-0',
						unit_price_cents: 100,
						line_total_cents: 100,
						loyalty_applied: false,
						freshness_confidence: 'high'
					}
				],
				total_cents: 400,
				missing_count: 0,
				dunnes_voucher_offer_text: null
			}
		},
		unresolved_items: [{ item_index: 2, entry_id: 'e-2', display_text: 'cream' }],
		no_valid_offer_items: [
			{
				item_index: 3,
				entry_id: 'e-3',
				display_text: 'truffle oil',
				canonical_item_id: 'ci-3',
				reason: 'no_matches'
			}
		],
		needs_confirmation_items: [],
		plain_language_explanation: 'Splitting between Tesco and Aldi saves you the most.',
		plan_explanations: [
			{
				plan: 'recommended',
				reasons: [
					'Two stores together cover your whole list.',
					'This is the cheapest way to buy everything today.'
				]
			},
			{
				plan: 'best_single',
				reasons: ['One trip covers your whole list.', 'No split between stores needed.']
			}
		],
		offer_snapshot_date: '2026-06-07',
		aldi_comparison: {
			fresh: {
				item_count: 1,
				aldi_total_cents: 250,
				recommended_total_cents: 400,
				saving_cents: 150
			},
			packaged: {
				item_count: 2,
				aldi_total_cents: 300,
				recommended_total_cents: 320,
				saving_cents: 20
			}
		}
	};
}

function responseWithFourPricedPlanLines(): OptimiseResponse {
	const response = fullResponse();
	response.recommended!.plan.lines = Array.from({ length: 4 }, (_, item_index) => ({
		item_index,
		entry_id: `e-${item_index}`,
		display_text: `Item ${item_index}`,
		product_name: `Product ${item_index}`,
		canonical_item_id: `ci-${item_index}`,
		retailer: 'tesco',
		retailer_product_id: `rp-${item_index}`,
		offer_id: `of-${item_index}`,
		unit_price_cents: 100,
		line_total_cents: 100,
		loyalty_applied: false,
		freshness_confidence: 'high'
	}));
	return response;
}

/**
 * Stub `fetch` for the optimise page. Always answers `/api/optimise` with
 * `response`/`status`. When `settings` is given it also answers `/api/settings`
 * (issue #98, for the occasional-store trip control); otherwise that path 404s
 * and the page simply shows no trip control.
 */
function stubOptimise(response: OptimiseResponse, status = 200, settings?: Settings): FetchMock {
	const fetchMock = vi.fn(async (input: RequestInfo | URL, _init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		if (settings && url.includes('/api/settings')) {
			return new Response(JSON.stringify(settings), { status: 200 });
		}
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
	function optimiseCallInit(fetchMock: FetchMock): RequestInit | undefined {
		const call = fetchMock.mock.calls.find(([input]) =>
			(typeof input === 'string' ? input : input.toString()).includes('/api/optimise')
		);
		return call?.[1];
	}

	/** Every parsed body of `POST /api/optimise` recorded by the mock. */
	function optimiseCallBodies(fetchMock: FetchMock): Record<string, unknown>[] {
		return fetchMock.mock.calls
			.filter(([input]) =>
				(typeof input === 'string' ? input : input.toString()).includes('/api/optimise')
			)
			.map(([, init]) => JSON.parse((init?.body as string) ?? '{}'));
	}

	it('POSTs to /api/optimise on mount with the list id', async () => {
		const fetchMock = stubOptimise(fullResponse());
		render(Page);

		// With an empty queue the flush is a no-op, so the optimise POST fires.
		await waitFor(() => expect(optimiseCallInit(fetchMock)).toBeDefined());
		const init = optimiseCallInit(fetchMock);
		expect(init?.method).toBe('POST');
		expect(JSON.parse(init?.body as string)).toEqual({ list_id: 'list-42', trip_retailers: [] });
	});

	it('offers a trip toggle for occasional stores and re-optimises with the trip', async () => {
		const fetchMock = stubOptimise(fullResponse(), 200, {
			convenience_threshold_2_cents: 1000,
			convenience_threshold_3_cents: 2500,
			categories: [],
			subcategories: [],
			stores: [
				{ retailer: 'tesco', status: 'active', nearby: false, category_rules: [] },
				{ retailer: 'dunnes', status: 'occasional', nearby: false, category_rules: [] },
				{ retailer: 'supervalu', status: 'active', nearby: true, category_rules: [] },
				{ retailer: 'aldi', status: 'active', nearby: false, category_rules: [] },
				{ retailer: 'lidl', status: 'off', nearby: false, category_rules: [] }
			]
		});
		render(Page);

		// Only the occasional Dunnes gets a toggle; active/off stores do not.
		const toggle = await screen.findByRole('button', { name: 'Dunnes' });
		expect(toggle.getAttribute('aria-pressed')).toBe('false');
		expect(screen.queryByRole('button', { name: 'Tesco' })).toBeNull();

		await fireEvent.click(toggle);

		await waitFor(() => {
			expect(
				optimiseCallBodies(fetchMock).some((body) =>
					Array.isArray(body.trip_retailers) &&
					(body.trip_retailers as string[]).includes('dunnes')
				)
			).toBe(true);
		});
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

	it('persists plan-specific misses in the chosen trip coverage', async () => {
		stubOptimise(fullResponse());
		render(Page);

		const chooseLinks = await screen.findAllByRole('link', { name: /shop this plan/i });
		document.addEventListener('click', (event) => event.preventDefault(), {
			capture: true,
			once: true
		});
		await fireEvent.click(chooseLinks[0]);

		const saved = JSON.parse(localStorage.getItem('buywise:trip:list-42')!) as ChosenTrip;
		expect(saved.allocation.unallocated).toContainEqual({
			entryId: 'e-1',
			reason: 'not_in_plan'
		});
		expect(new Set(saved.allocation.entryIds)).toEqual(new Set(['e-0', 'e-1', 'e-2', 'e-3']));
	});

	it('renders the plain-language explanation', async () => {
		stubOptimise(fullResponse());
		render(Page);
		await screen.findByText(/Splitting between Tesco and Aldi/);
	});

	it('renders each plan card’s reasons, scoped to its own card', async () => {
		stubOptimise(fullResponse());
		render(Page);

		// The recommended card carries its own reasons, in server order. Scoping to
		// the card proves the lookup matched the right plan kind (rather than every
		// card getting every plan's reasons).
		const recommended = await screen.findByLabelText(/recommended/i);
		const reasons = within(recommended).getByRole('list', { name: 'Why this plan' });
		expect(within(reasons).getAllByRole('listitem').map((li) => li.textContent)).toEqual([
			'Two stores together cover your whole list.',
			'This is the cheapest way to buy everything today.'
		]);
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

	it('renders a chooser for ask-gated items and the strict no-offer reason', async () => {
		const response = fullResponse();
		response.needs_confirmation_items = [
			{
				item_index: 4,
				entry_id: 'e-4',
				display_text: 'yoghurt',
				canonical_item_id: 'ci-yoghurt',
				candidate_count: 2,
				candidates: [
					{
						retailer_product_id: 'rp-y-tesco',
						name: 'Glenisk Natural',
						brand: 'Glenisk',
						size_text: '500g',
						price_cents: 220,
						retailer: 'tesco'
					},
					{
						retailer_product_id: 'rp-y-aldi',
						name: 'Brooklea Natural',
						brand: 'Brooklea',
						size_text: '500g',
						price_cents: 180,
						retailer: 'aldi'
					}
				]
			}
		];
		response.no_valid_offer_items = [
			{
				item_index: 5,
				entry_id: 'e-5',
				display_text: 'San Pellegrino',
				canonical_item_id: 'ci-5',
				reason: 'strict_no_exact_offer'
			},
			{
				item_index: 6,
				entry_id: 'e-6',
				display_text: 'Apples',
				canonical_item_id: 'ci-6',
				reason: 'store_category_rules_excluded_all'
			}
		];
		stubOptimise(response);
		render(Page);

		// substitution_policy=ask items get their own confirmation section that now
		// renders a real chooser (issue #78): the item name, each candidate's
		// details, and a Choose button per candidate.
		const confirm = await screen.findByLabelText(/awaiting your confirmation/i);
		const text = confirm.textContent?.replace(/\s+/g, ' ') ?? '';
		expect(text).toMatch(/yoghurt/);
		expect(text).toMatch(/Glenisk Natural/);
		expect(text).toMatch(/Brooklea Natural/);
		expect(text).toMatch(/€2\.20/);
		expect(text).toMatch(/€1\.80/);
		// One Choose button per candidate; the old "coming soon" copy is gone.
		expect(screen.getAllByRole('button', { name: /choose/i })).toHaveLength(2);
		expect(text).not.toMatch(/coming soon/i);

		// strict_no_exact_offer maps to human copy, never the raw wire value.
		expect(screen.getByText(/San Pellegrino — the exact product has no current offer/)).toBeTruthy();
		expect(screen.getByText(/Apples — store category rules excluded every option/)).toBeTruthy();
		expect(screen.queryByText(/strict_no_exact_offer/)).toBeNull();
	});

	it('confirms a candidate via match-feedback prefer, then re-optimises', async () => {
		const { fireEvent } = await import('@testing-library/svelte');

		// First optimise returns one ask item; after the prefer it returns plans
		// with no ask items left (the confirmed item is now priced).
		const withAsk = fullResponse();
		withAsk.recommended = null;
		withAsk.best_single = null;
		withAsk.best_two_store = null;
		withAsk.theoretical_minimum = null;
		withAsk.unresolved_items = [];
		withAsk.no_valid_offer_items = [];
		withAsk.needs_confirmation_items = [
			{
				item_index: 0,
				entry_id: 'e-0',
				display_text: 'yoghurt',
				canonical_item_id: 'ci-yoghurt',
				candidate_count: 1,
				candidates: [
					{
						retailer_product_id: 'rp-y-aldi',
						name: 'Brooklea Natural',
						brand: 'Brooklea',
						size_text: '500g',
						price_cents: 180,
						retailer: 'aldi'
					}
				]
			}
		];
		const afterConfirm = fullResponse(); // plans present, no ask items

		let optimiseCalls = 0;
		const feedbackBodies: unknown[] = [];
		const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.includes('/api/match-feedback')) {
				feedbackBodies.push(JSON.parse(String(init?.body)));
				return new Response(
					JSON.stringify({ product_match_id: 'pm-1', status: 'accepted', action: 'prefer' }),
					{ status: 201 }
				);
			}
			if (url.includes('/api/optimise')) {
				optimiseCalls += 1;
				const body = optimiseCalls === 1 ? withAsk : afterConfirm;
				return new Response(JSON.stringify(body), { status: 200 });
			}
			return new Response('not found', { status: 404 });
		});
		vi.stubGlobal('fetch', fetchMock);
		render(Page);

		// The chooser appears; click Choose on the only candidate.
		const choose = await screen.findByRole('button', { name: /choose/i });
		await fireEvent.click(choose);

		// The prefer choice was POSTed with the item + chosen product.
		await waitFor(() => expect(feedbackBodies).toHaveLength(1));
		expect(feedbackBodies[0]).toEqual({
			canonical_item_id: 'ci-yoghurt',
			retailer_product_id: 'rp-y-aldi',
			action: 'prefer'
		});

		// And the page re-optimised: the second response (with plans, no ask items)
		// is rendered, so the explanation copy appears and the chooser is gone.
		await screen.findByText(/Splitting between Tesco and Aldi/);
		expect(screen.queryByRole('button', { name: /choose/i })).toBeNull();
		expect(optimiseCalls).toBe(2);
	});

	it('drops a stale re-optimise when a newer confirmation supersedes it', async () => {
		// Race regression: with two ask items, the household can confirm A then B
		// before A's re-optimise resolves (only the in-flight item's own buttons
		// disable). Both runs target the same list, so the `id !== listId` guard
		// can't catch it — if A's older response lands last it would clobber B's
		// newer one and wrongly re-show a chooser. The request-sequence guard must
		// drop the superseded run.
		const { fireEvent } = await import('@testing-library/svelte');

		function deferred<T>() {
			let resolve!: (value: T) => void;
			const promise = new Promise<T>((r) => {
				resolve = r;
			});
			return { promise, resolve };
		}

		// Two ask items, one candidate each → two "Choose" buttons.
		const twoAsk: OptimiseResponse = {
			basket_items: [],
			run_id: 'run-ask',
			best_single: null,
			best_two_store: null,
			theoretical_minimum: null,
			recommended: null,
			unresolved_items: [],
			no_valid_offer_items: [],
			needs_confirmation_items: [
				{
					item_index: 0,
					entry_id: 'e-0',
					display_text: 'yoghurt',
					canonical_item_id: 'ci-a',
					candidate_count: 1,
					candidates: [
						{
							retailer_product_id: 'rp-a',
							name: 'Brooklea Natural',
							brand: 'Brooklea',
							size_text: '500g',
							price_cents: 180,
							retailer: 'aldi'
						}
					]
				},
				{
					item_index: 1,
					entry_id: 'e-1',
					display_text: 'butter',
					canonical_item_id: 'ci-b',
					candidate_count: 1,
					candidates: [
						{
							retailer_product_id: 'rp-b',
							name: 'Kerrygold',
							brand: 'Kerrygold',
							size_text: '250g',
							price_cents: 300,
							retailer: 'tesco'
						}
					]
				}
			],
			plain_language_explanation: '',
			plan_explanations: [],
			offer_snapshot_date: null,
			aldi_comparison: null
		};

		// A's re-optimise (older, seq 2): butter still ask. Tagged so we can detect
		// it leaking into the DOM.
		const staleAfterA: OptimiseResponse = {
			...twoAsk,
			run_id: 'run-stale-a',
			needs_confirmation_items: [twoAsk.needs_confirmation_items[1]],
			plain_language_explanation: 'STALE: only yoghurt confirmed'
		};

		// B's re-optimise (newer, seq 3): both confirmed → plans, no ask items.
		const bothConfirmed = fullResponse();

		let optimiseCall = 0;
		let feedbackCall = 0;
		const fbA = deferred<Response>();
		const fbB = deferred<Response>();
		const optA = deferred<Response>();
		const optB = deferred<Response>();
		const acceptedFeedback = () =>
			new Response(JSON.stringify({ product_match_id: 'pm', status: 'accepted', action: 'prefer' }), {
				status: 201
			});

		const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.includes('/api/match-feedback')) {
				feedbackCall += 1;
				return feedbackCall === 1 ? fbA.promise : fbB.promise;
			}
			if (url.includes('/api/optimise')) {
				optimiseCall += 1;
				if (optimiseCall === 1) return new Response(JSON.stringify(twoAsk), { status: 200 });
				return optimiseCall === 2 ? optA.promise : optB.promise;
			}
			return new Response('not found', { status: 404 });
		});
		vi.stubGlobal('fetch', fetchMock);
		render(Page);

		// Both choosers render.
		await waitFor(() => expect(screen.getAllByRole('button', { name: /choose/i })).toHaveLength(2));
		const buttons = screen.getAllByRole('button', { name: /choose/i });

		// Confirm A, then B, while A's prefer POST is still in flight.
		await fireEvent.click(buttons[0]); // ci-a
		await fireEvent.click(buttons[1]); // ci-b
		await waitFor(() => expect(feedbackCall).toBe(2));

		// A's prefer resolves first: its re-optimise reaches the network (seq 2) and
		// stays in flight (deferred), so it is genuinely racing, not dropped early.
		fbA.resolve(acceptedFeedback());
		await waitFor(() => expect(optimiseCall).toBe(2));
		// Then B's prefer resolves: a newer re-optimise starts (seq 3).
		fbB.resolve(acceptedFeedback());
		await waitFor(() => expect(optimiseCall).toBe(3));

		// The newer run (B) resolves first and renders plans…
		optB.resolve(new Response(JSON.stringify(bothConfirmed), { status: 200 }));
		await screen.findByText(/Splitting between Tesco and Aldi/);

		// …then the older run (A) lands last. The sequence guard must drop it, so
		// the stale chooser/explanation never reappears.
		optA.resolve(new Response(JSON.stringify(staleAfterA), { status: 200 }));
		await new Promise((r) => setTimeout(r, 0));

		expect(screen.queryByText(/STALE/)).toBeNull();
		expect(screen.queryByRole('button', { name: /choose/i })).toBeNull();
		expect(screen.getByText(/Splitting between Tesco and Aldi/)).toBeTruthy();
	});

	it('tracks busy state per ask item so a second confirmation does not re-enable the first', async () => {
		// Concurrency regression: each ask item disables only its own buttons, so the
		// in-flight set must be keyed per canonical id. A single shared busy flag let
		// starting item B re-enable item A's button (and A's completion clear B's),
		// risking duplicate `prefer` POSTs. Hold both prefer POSTs so the chooser
		// stays up and both items are concurrently in flight.
		const { fireEvent } = await import('@testing-library/svelte');

		const twoAsk: OptimiseResponse = {
			basket_items: [],
			run_id: 'run-ask',
			best_single: null,
			best_two_store: null,
			theoretical_minimum: null,
			recommended: null,
			unresolved_items: [],
			no_valid_offer_items: [],
			needs_confirmation_items: [
				{
					item_index: 0,
					entry_id: 'e-0',
					display_text: 'yoghurt',
					canonical_item_id: 'ci-a',
					candidate_count: 1,
					candidates: [
						{
							retailer_product_id: 'rp-a',
							name: 'Brooklea Natural',
							brand: 'Brooklea',
							size_text: '500g',
							price_cents: 180,
							retailer: 'aldi'
						}
					]
				},
				{
					item_index: 1,
					entry_id: 'e-1',
					display_text: 'butter',
					canonical_item_id: 'ci-b',
					candidate_count: 1,
					candidates: [
						{
							retailer_product_id: 'rp-b',
							name: 'Kerrygold',
							brand: 'Kerrygold',
							size_text: '250g',
							price_cents: 300,
							retailer: 'tesco'
						}
					]
				}
			],
			plain_language_explanation: '',
			plan_explanations: [],
			offer_snapshot_date: null,
			aldi_comparison: null
		};

		let feedbackCalls = 0;
		const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.includes('/api/match-feedback')) {
				feedbackCalls += 1;
				return new Promise<Response>(() => {}); // never resolves → stays in flight
			}
			if (url.includes('/api/optimise')) {
				return new Response(JSON.stringify(twoAsk), { status: 200 });
			}
			return new Response('not found', { status: 404 });
		});
		vi.stubGlobal('fetch', fetchMock);
		render(Page);

		const buttons = (await screen.findAllByRole('button', {
			name: /choose/i
		})) as HTMLButtonElement[];
		expect(buttons).toHaveLength(2);

		// Confirm A: its button goes busy/disabled.
		await fireEvent.click(buttons[0]);
		await waitFor(() => expect(buttons[0].disabled).toBe(true));

		// Confirm B: its button goes busy too…
		await fireEvent.click(buttons[1]);
		await waitFor(() => expect(buttons[1].disabled).toBe(true));

		// …and A stays disabled (its POST is still in flight). The old single-id flag
		// would have re-enabled A here.
		expect(buttons[0].disabled).toBe(true);
		expect(feedbackCalls).toBe(2);
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
			basket_items: [],
			run_id: 'run-empty',
			best_single: null,
			best_two_store: null,
			theoretical_minimum: null,
			recommended: null,
			unresolved_items: [],
			no_valid_offer_items: [],
			needs_confirmation_items: [],
			plain_language_explanation: '',
			plan_explanations: [],
			offer_snapshot_date: null,
			aldi_comparison: null
		};
		stubOptimise(empty);
		render(Page);
		await screen.findByText(/no plans to show/i);
	});

	it('does not claim the list is empty when every item is ask-gated', async () => {
		// All items held for confirmation → optimiser returns no plans, but the
		// list is not empty. The generic "add some items" copy would be wrong.
		const allHeld: OptimiseResponse = {
			basket_items: [{ item_index: 0, entry_id: 'e-0', display_text: 'yoghurt' }],
			run_id: 'run-held',
			best_single: null,
			best_two_store: null,
			theoretical_minimum: null,
			recommended: null,
			unresolved_items: [],
			no_valid_offer_items: [],
			needs_confirmation_items: [
				{
					item_index: 0,
					entry_id: 'e-0',
					display_text: 'yoghurt',
					canonical_item_id: 'ci-yoghurt',
					candidate_count: 2,
					candidates: [
						{
							retailer_product_id: 'rp-y-tesco',
							name: 'Glenisk Natural',
							brand: 'Glenisk',
							size_text: '500g',
							price_cents: 220,
							retailer: 'tesco'
						},
						{
							retailer_product_id: 'rp-y-aldi',
							name: 'Brooklea Natural',
							brand: 'Brooklea',
							size_text: '500g',
							price_cents: 180,
							retailer: 'aldi'
						}
					]
				}
			],
			plain_language_explanation: '',
			plan_explanations: [],
			offer_snapshot_date: null,
			aldi_comparison: null
		};
		stubOptimise(allHeld);
		render(Page);

		await screen.findByText(/some items need confirmation or review below/i);
		expect(screen.queryByText(/add some items to your list/i)).toBeNull();
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

	it('shows the Aldi comparison split fresh versus packaged', async () => {
		stubOptimise(responseWithFourPricedPlanLines());
		render(Page);

		const section = await screen.findByRole('region', { name: 'Aldi comparison' });
		// Total saving is the sum of both splits and covers 3 of 4 priced plan items.
		expect(
			within(section).getByText(
				/Aldi would have saved €1\.70 on this list \(covers 3 of 4 plan items\)/
			)
		).toBeTruthy();
		expect(within(section).getByText(/Fresh food: €1\.50 cheaper/)).toBeTruthy();
		expect(within(section).getByText(/Packaged food: €0\.20 cheaper/)).toBeTruthy();
		// The section states it never affects the recommendation.
		expect(within(section).getByText(/never changes which plan is recommended/)).toBeTruthy();
	});

	it('shows comparison coverage when Aldi would cost more', async () => {
		const response = responseWithFourPricedPlanLines();
		response.aldi_comparison!.packaged.saving_cents = -200;
		stubOptimise(response);
		render(Page);

		const section = await screen.findByRole('region', { name: 'Aldi comparison' });
		expect(
			within(section).getByText(
				/Aldi would have cost €0\.50 more than your plan \(covers 3 of 4 plan items\)/
			)
		).toBeTruthy();
	});

	it('says there is nothing to compare when Aldi prices none of the plan', async () => {
		const response = fullResponse();
		response.aldi_comparison = {
			fresh: { item_count: 0, aldi_total_cents: 0, recommended_total_cents: 0, saving_cents: 0 },
			packaged: { item_count: 0, aldi_total_cents: 0, recommended_total_cents: 0, saving_cents: 0 }
		};
		stubOptimise(response);
		render(Page);

		const section = await screen.findByRole('region', { name: 'Aldi comparison' });
		expect(within(section).getByText(/nothing to compare/i)).toBeTruthy();
	});

	it('renders no comparison section without a recommended plan', async () => {
		const response = fullResponse();
		response.aldi_comparison = null;
		stubOptimise(response);
		render(Page);

		await screen.findByRole('region', { name: 'Plans' });
		expect(screen.queryByRole('region', { name: 'Aldi comparison' })).toBeNull();
	});
});
