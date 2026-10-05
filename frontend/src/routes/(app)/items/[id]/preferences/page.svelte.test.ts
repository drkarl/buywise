import { afterEach, describe, expect, it, vi, type Mock } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import type { CandidateView, Preferences } from '$lib/api/preferences';

// `$app/state` is provided by SvelteKit at runtime; mock it so the page can read
// the route id (and the optional back-to-list param) without a kit server.
vi.mock('$app/state', () => ({
	page: {
		params: { id: 'ci-beans' },
		url: new URL('http://localhost/items/ci-beans/preferences?list=list-a')
	}
}));

import Page from './+page.svelte';

type FetchMock = Mock<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>;

/** A preferences payload: brand rule set, one accepted Heinz product. */
function basePreferences(): Preferences {
	return {
		canonical_item_id: 'ci-beans',
		name_en: 'Baked beans',
		name_es: 'Judías cocidas',
		default_constraint_policy: 'any',
		constraints: {
			brand: null,
			allowed_brands: [],
			excluded_brands: [],
			preferred_size_value: null,
			preferred_size_basis: null,
			substitution_policy: 'any'
		},
		matches: [
			{
				retailer_product_id: 'rp-heinz-tesco',
				status: 'accepted',
				matched_by: 'system',
				name: 'Heinz Baked Beans',
				brand: 'Heinz',
				retailer: 'tesco',
				size_text: '415g'
			}
		],
		candidates: [],
		warnings: []
	};
}

/** A candidate payload for the "Suggested products" section. */
function candidate(overrides: Partial<CandidateView> = {}): CandidateView {
	return {
		retailer_product_id: 'rp-own-aldi',
		retailer: 'aldi',
		name: 'Own Brand Baked Beans',
		brand: 'Aldi',
		size_text: '420g',
		pack_count: 1,
		score: 70,
		signals: ['name'],
		price_cents: 85,
		loyalty_price_cents: null,
		loyalty_scheme: null,
		unit_price_eur_micros: null,
		unit_price_basis: null,
		offer_snapshot_date: '2026-06-01',
		fetched_at: '2026-06-02T00:00:00.000Z',
		source_confidence: 'high',
		...overrides
	};
}

function jsonResponse(body: unknown, status = 200): Response {
	return new Response(JSON.stringify(body), {
		status,
		headers: { 'Content-Type': 'application/json' }
	});
}

/** Stub fetch: GET/PUT `/preferences` and POST `/api/match-feedback`. */
function stubPreferences(preferences: Preferences): FetchMock {
	const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = String(input);
		if (url.endsWith('/preferences') && init?.method === 'PUT') {
			const body = JSON.parse(init.body as string) as Record<string, unknown>;
			return jsonResponse({
				...preferences,
				default_constraint_policy: body.default_constraint_policy,
				constraints: {
					brand: body.brand,
					allowed_brands: body.allowed_brands,
					excluded_brands: body.excluded_brands,
					preferred_size_value: body.preferred_size_value,
					preferred_size_basis: body.preferred_size_basis,
					substitution_policy: body.substitution_policy
				}
			});
		}
		if (url.endsWith('/preferences')) {
			return jsonResponse(preferences);
		}
		if (url === '/api/match-feedback') {
			return jsonResponse({ product_match_id: 'm-1', status: 'rejected', action: 'reject' }, 201);
		}
		throw new Error(`unexpected fetch: ${url}`);
	});
	vi.stubGlobal('fetch', fetchMock);
	return fetchMock as unknown as FetchMock;
}

/** The parsed body of the PUT request the page made, if any. */
function putBody(fetchMock: FetchMock): Record<string, unknown> | null {
	const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'PUT');
	if (!call) return null;
	return JSON.parse(call[1]!.body as string);
}

afterEach(() => {
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
});

describe('item preferences', () => {
	it('loads and renders the rule and the known products', async () => {
		stubPreferences(basePreferences());
		render(Page);

		await waitFor(() => expect(screen.getByText('Baked beans')).toBeTruthy());
		expect(screen.getByText('Heinz Baked Beans')).toBeTruthy();
		expect(screen.getByLabelText('Rule')).toBeTruthy();
	});

	it('saves a brand rule with a preferred size', async () => {
		const fetchMock = stubPreferences(basePreferences());
		render(Page);
		await waitFor(() => expect(screen.getByLabelText('Rule')).toBeTruthy());

		await fireEvent.change(screen.getByLabelText('Rule'), { target: { value: 'brand' } });
		await fireEvent.input(screen.getByLabelText('Brand'), { target: { value: 'Heinz' } });
		await fireEvent.input(screen.getByLabelText('Size'), { target: { value: '415' } });
		await fireEvent.change(screen.getByLabelText('Size unit'), { target: { value: 'g' } });
		await fireEvent.click(screen.getByText('Save preferences'));

		await waitFor(() => {
			const body = putBody(fetchMock);
			expect(body).not.toBeNull();
			expect(body!.default_constraint_policy).toBe('brand');
			expect(body!.brand).toBe('Heinz');
			expect(body!.preferred_size_value).toBe(415);
			expect(body!.preferred_size_basis).toBe('g');
		});
	});

	it('clears allowed brands when switching to any product', async () => {
		const preferences = basePreferences();
		preferences.default_constraint_policy = 'allowed_set';
		preferences.constraints.allowed_brands = ['Branston'];
		const fetchMock = stubPreferences(preferences);
		render(Page);
		await waitFor(() => expect(screen.getByLabelText('Rule')).toBeTruthy());

		await fireEvent.change(screen.getByLabelText('Rule'), { target: { value: 'any' } });
		await fireEvent.click(screen.getByText('Save preferences'));

		await waitFor(() => expect(putBody(fetchMock)?.allowed_brands).toEqual([]));
	});

	it('labels pending matches as awaiting review', async () => {
		const preferences = basePreferences();
		preferences.matches[0].status = 'pending';
		stubPreferences(preferences);
		render(Page);

		await waitFor(() => expect(screen.getByText('Awaiting review')).toBeTruthy());
		expect(screen.queryByText('Allowed')).toBeNull();
	});

	it('refuses to save a brand rule without a brand', async () => {
		const fetchMock = stubPreferences(basePreferences());
		render(Page);
		await waitFor(() => expect(screen.getByLabelText('Rule')).toBeTruthy());

		await fireEvent.change(screen.getByLabelText('Rule'), { target: { value: 'brand' } });
		await fireEvent.click(screen.getByText('Save preferences'));

		await waitFor(() =>
			expect(screen.getByText('Choose a brand, or pick a different rule.')).toBeTruthy()
		);
		expect(putBody(fetchMock)).toBeNull();
	});

	it('rejects a product and re-reads the preferences', async () => {
		const fetchMock = stubPreferences(basePreferences());
		render(Page);
		await waitFor(() => expect(screen.getByText('Reject')).toBeTruthy());

		await fireEvent.click(screen.getByText('Reject'));

		await waitFor(() => {
			const feedback = fetchMock.mock.calls.find(([input]) =>
				String(input).endsWith('/api/match-feedback')
			);
			expect(feedback).toBeTruthy();
			const body = JSON.parse(feedback![1]!.body as string);
			expect(body.action).toBe('reject');
			expect(body.canonical_item_id).toBe('ci-beans');
			expect(body.retailer_product_id).toBe('rp-heinz-tesco');
		});
	});

	it('renders a suggested candidate with its price and provenance', async () => {
		const preferences = basePreferences();
		preferences.candidates = [candidate()];
		stubPreferences(preferences);
		render(Page);

		await waitFor(() => expect(screen.getByText('Own Brand Baked Beans')).toBeTruthy());
		expect(screen.getByText('€0.85')).toBeTruthy();
		expect(screen.getByText(/price seen 2026-06-01/)).toBeTruthy();
		expect(screen.getByText('name match')).toBeTruthy();
	});

	it('matches a suggested candidate', async () => {
		const preferences = basePreferences();
		preferences.candidates = [candidate()];
		const fetchMock = stubPreferences(preferences);
		render(Page);
		await waitFor(() => expect(screen.getByText('Own Brand Baked Beans')).toBeTruthy());

		await fireEvent.click(screen.getByText('Match'));

		await waitFor(() => {
			const feedback = fetchMock.mock.calls.find(([input]) =>
				String(input).endsWith('/api/match-feedback')
			);
			expect(feedback).toBeTruthy();
			const body = JSON.parse(feedback![1]!.body as string);
			expect(body.action).toBe('accept');
			expect(body.retailer_product_id).toBe('rp-own-aldi');
		});
	});

	it('undoes a rejected match, restoring it for review', async () => {
		const preferences = basePreferences();
		preferences.matches[0].status = 'rejected';
		const fetchMock = stubPreferences(preferences);
		render(Page);
		await waitFor(() => expect(screen.getByText('Undo')).toBeTruthy());

		await fireEvent.click(screen.getByText('Undo'));

		await waitFor(() => {
			const feedback = fetchMock.mock.calls.find(([input]) =>
				String(input).endsWith('/api/match-feedback')
			);
			expect(feedback).toBeTruthy();
			const body = JSON.parse(feedback![1]!.body as string);
			expect(body.action).toBe('undo');
			expect(body.retailer_product_id).toBe('rp-heinz-tesco');
		});
	});
});
