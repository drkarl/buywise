import { afterEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/svelte';
import type { SavingsSummary } from '$lib/api/savings';

import Page from './+page.svelte';

const META = {
	baseline_label: 'Baseline: best single store estimate',
	costs_note: 'Travel included where entered; service and data costs excluded.'
};

/** Null covered fields for an event whose run predates the frozen snapshot. */
const NO_COVERAGE = {
	full_basket_saving_cents: null,
	covered_subtotal_cents: null,
	covered_saving_cents: null,
	covered_lines: null,
	total_lines: null,
	price_date: null,
	covered_label: null
};

/**
 * A summary with an empty verified aggregate (nothing is reconciled yet), a
 * populated legacy total-only variance, and one event of each honest status.
 */
function fullSummary(): SavingsSummary {
	return {
		saved: {
			label: 'Verified savings',
			note: 'Only line-reconciled results count as saved. There are none yet.',
			...META,
			cumulative_cents: 0,
			monthly: []
		},
		legacy: {
			label: 'Legacy total-only basket variance',
			note: 'Whole-basket actual spend recorded against the best single store estimate; quantities and purchases unverified.',
			...META,
			cumulative_cents: 200,
			monthly: [
				{
					month: '2026-05',
					baseline_cents: 5000,
					manual_total_cents: 4500,
					travel_cents: 300,
					variance_cents: 200,
					lists: 1
				}
			]
		},
		lists: [
			{
				id: 'ev-3',
				list_id: 'list-3',
				list_name: 'June shop',
				archived_at: '2026-06-01T10:00:00Z',
				status: 'estimated',
				status_label: 'Estimated opportunity (not verified)',
				coverage_label: 'Both plans price the same lines; quantities not reconciled',
				...META,
				...NO_COVERAGE,
				full_basket_saving_cents: 2000,
				gross_saving_cents: 2000,
				chosen_basket_cost_cents: null,
				basket_variance_cents: null
			},
			{
				id: 'ev-2',
				list_id: 'list-2',
				list_name: 'Unreconciled shop',
				archived_at: '2026-04-03T10:00:00Z',
				status: 'unreconciled',
				status_label: 'Not comparable — plans price different lines',
				coverage_label: 'No saving claimed: the plans price different lines',
				...META,
				...NO_COVERAGE,
				covered_subtotal_cents: 800,
				covered_saving_cents: 120,
				covered_lines: 4,
				total_lines: 6,
				price_date: '2026-04-01',
				covered_label: 'Covered subtotal: 4 of 6 lines, prices as of 2026-04-01',
				gross_saving_cents: 999,
				chosen_basket_cost_cents: null,
				basket_variance_cents: null
			},
			{
				id: 'ev-1',
				list_id: 'list-1',
				list_name: 'May shop',
				archived_at: '2026-05-03T10:00:00Z',
				status: 'legacy_total_only',
				status_label: 'Legacy total-only basket variance',
				coverage_label: 'Whole-basket total only; quantities and purchases unverified',
				...META,
				...NO_COVERAGE,
				gross_saving_cents: 3000,
				chosen_basket_cost_cents: 4500,
				basket_variance_cents: 200
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
	it('labels unchecked historical comparability without claiming a saving', async () => {
		const summary = fullSummary();
		summary.lists.push({
			id: 'ev-unchecked',
			list_id: 'list-unchecked',
			list_name: 'Unchecked shop',
			archived_at: '2026-03-01T10:00:00Z',
			status: 'unchecked',
			status_label: 'Comparability not checked',
			coverage_label: 'No saving claimed: comparability not checked',
			...META,
			...NO_COVERAGE,
			gross_saving_cents: 2000,
			chosen_basket_cost_cents: null,
			basket_variance_cents: null
		});
		stubSummary(summary);
		render(Page);

		expect(await screen.findByText('Unchecked shop')).toBeTruthy();
		expect(screen.getByText('Comparability not checked')).toBeTruthy();
		expect(screen.getByText(/No saving claimed: comparability not checked/)).toBeTruthy();
	});

	it('claims no saved money and shows the verified empty state', async () => {
		stubSummary(fullSummary());
		render(Page);

		const region = await screen.findByRole('region', { name: 'Verified savings' });
		expect(within(region).getByText('Verified savings')).toBeTruthy();
		expect(within(region).getByText('€0.00')).toBeTruthy();
		expect(
			within(region).getByText('Only line-reconciled results count as saved. There are none yet.')
		).toBeTruthy();
		// The legacy variance (€2.00) must never appear inside the saved aggregate.
		expect(within(region).queryByText('€2.00')).toBeNull();
		expect(
			screen.getByText('No verified savings by month yet — nothing here is counted as saved.')
		).toBeTruthy();
	});

	it('shows the legacy total-only variance under its own label with its figures', async () => {
		stubSummary(fullSummary());
		render(Page);

		const legacy = await screen.findByRole('region', {
			name: 'Legacy total-only basket variance'
		});
		expect(within(legacy).getByText('Legacy total-only basket variance')).toBeTruthy();
		expect(within(legacy).getAllByText('€2.00')).toHaveLength(2);
		// Month bucket grouped by the archive date, with the original figures.
		expect(within(legacy).getByText('May 2026')).toBeTruthy();
		expect(within(legacy).getByText('Best-single estimate')).toBeTruthy();
		expect(within(legacy).getByText('€50.00')).toBeTruthy();
		expect(within(legacy).getByText('Manual total')).toBeTruthy();
		expect(within(legacy).getByText('€45.00')).toBeTruthy();
		expect(within(legacy).getByText('Travel')).toBeTruthy();
		expect(within(legacy).getByText('€3.00')).toBeTruthy();
		expect(within(legacy).getByText('Total-only variance')).toBeTruthy();
		expect(within(legacy).queryByText('Gross estimate')).toBeNull();
	});

	it('labels every event with its status, baseline, coverage and costs', async () => {
		stubSummary(fullSummary());
		render(Page);

		await waitFor(() => expect(screen.getByText('June shop')).toBeTruthy());
		// One honest status label per event — never "saved" for an estimate.
		expect(screen.getByText('Estimated opportunity (not verified)')).toBeTruthy();
		expect(screen.getByText('Not comparable — plans price different lines')).toBeTruthy();
		expect(screen.getAllByText('Legacy total-only basket variance').length).toBeGreaterThan(0);
		// Coverage copy distinguishes a like-for-like estimate from a refusal.
		expect(
			screen.getByText(/Both plans price the same lines; quantities not reconciled/)
		).toBeTruthy();
		expect(screen.getByText(/No saving claimed: the plans price different lines/)).toBeTruthy();
		// Both plans price the same lines (estimate) and the refused comparison
		// carry their baseline and included-costs copy: one meta line on each
		// aggregate plus one on each of the three event cards.
		expect(screen.getAllByText(/Baseline: best single store estimate/)).toHaveLength(5);
	});

	it('shows the "Enter actual spend" prompt only where no total was entered', async () => {
		stubSummary(fullSummary());
		render(Page);

		await waitFor(() => expect(screen.getByText('June shop')).toBeTruthy());
		const links = screen.getAllByRole('link', { name: 'Enter actual spend' });
		expect(links).toHaveLength(2);
		expect(links.map((l) => l.getAttribute('href')).sort()).toEqual([
			'/list/list-2',
			'/list/list-3'
		]);
		// The legacy event already has a total: it shows the variance, not the prompt.
		expect(screen.getByText('Basket variance €2.00')).toBeTruthy();
	});

	it('renders empty-state copy when there are no events at all', async () => {
		stubSummary({
			saved: {
				label: 'Verified savings',
				note: 'Only line-reconciled results count as saved. There are none yet.',
				...META,
				cumulative_cents: 0,
				monthly: []
			},
			legacy: {
				label: 'Legacy total-only basket variance',
				note: 'Whole-basket actual spend recorded against the best single store estimate; quantities and purchases unverified.',
				...META,
				cumulative_cents: 0,
				monthly: []
			},
			lists: []
		});
		render(Page);

		const region = await screen.findByRole('region', { name: 'Verified savings' });
		expect(within(region).getByText('€0.00')).toBeTruthy();
		expect(
			screen.getByText('Your archived lists will appear here with their outcomes.')
		).toBeTruthy();
	});

	it('shows a bounded covered subtotal instead of a saving for an unreconciled event', async () => {
		stubSummary(fullSummary());
		render(Page);

		await waitFor(() => expect(screen.getByText('Unreconciled shop')).toBeTruthy());
		// The frozen-coverage label names the coverage and evidence date, and the
		// covered subtotal replaces the suppressed full-basket figure.
		expect(
			screen.getByText(/Covered subtotal: 4 of 6 lines, prices as of 2026-04-01/)
		).toBeTruthy();
		expect(screen.getByText(/bounded estimate, not a saving/)).toBeTruthy();
		expect(screen.queryByText(/Stored figure €9.99/)).toBeNull();
	});

	it('shows an error with a retry on a non-OK response', async () => {
		stubSummary(fullSummary(), 500);
		render(Page);
		const alert = await screen.findByRole('alert');
		expect(alert.textContent).toMatch(/couldn’t load your savings/i);
		expect(screen.getByRole('button', { name: 'Try again' })).toBeTruthy();
	});
});
