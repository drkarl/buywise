import { describe, expect, it } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import PlanCard from './PlanCard.svelte';
import type { StorePlan } from '$lib/api/optimise';

/** A realistic two-line single-store plan matching the wire contract. */
function mockPlan(overrides: Partial<StorePlan> = {}): StorePlan {
	return {
		stores: ['tesco'],
		lines: [
			{
				item_index: 0,
				display_text: 'Milk 2L',
				canonical_item_id: 'ci-1',
				retailer: 'tesco',
				retailer_product_id: 'rp-1',
				offer_id: 'of-1',
				unit_price_cents: 199,
				line_total_cents: 199,
				loyalty_applied: false,
				freshness_confidence: 'high'
			},
			{
				item_index: 1,
				display_text: 'Bread',
				canonical_item_id: 'ci-2',
				retailer: 'tesco',
				retailer_product_id: 'rp-2',
				offer_id: 'of-2',
				unit_price_cents: 150,
				line_total_cents: 150,
				loyalty_applied: true,
				freshness_confidence: 'unknown'
			}
		],
		total_cents: 349,
		missing_count: 0,
		dunnes_voucher_offer_text: null,
		...overrides
	};
}

describe('PlanCard', () => {
	it('displays the total formatted as euros', () => {
		render(PlanCard, { plan: mockPlan(), title: 'Best single store' });
		// 349 cents → €3.49 (acceptance criterion: correct total).
		expect(screen.getByText('€3.49')).toBeTruthy();
	});

	it('shows the store name and the title', () => {
		render(PlanCard, { plan: mockPlan(), title: 'Best single store' });
		// "Tesco" also appears as a per-line store label, so target the heading +
		// the joined store header rather than a bare text lookup.
		expect(screen.getByRole('heading', { name: 'Best single store' })).toBeTruthy();
		expect(screen.getAllByText('Tesco').length).toBeGreaterThan(0);
	});

	it('joins multiple stores for a split plan', () => {
		const plan = mockPlan({ stores: ['tesco', 'aldi'] });
		render(PlanCard, { plan, title: 'Cheapest split' });
		expect(screen.getByText('Tesco + Aldi')).toBeTruthy();
	});

	it('marks the recommended plan as visually distinct', () => {
		render(PlanCard, { plan: mockPlan(), title: 'Recommended store', recommended: true });
		// The "Recommended" badge plus an accessible name that flags it.
		expect(screen.getByText('Recommended')).toBeTruthy();
		expect(screen.getByLabelText(/recommended/i)).toBeTruthy();
	});

	it('renders a confidence indicator for the `unknown` line only', () => {
		render(PlanCard, { plan: mockPlan(), title: 'Best single store' });
		// Two lines, one `high` (no badge) + one `unknown` (badge) → exactly one.
		const badges = screen.getAllByRole('img');
		expect(badges).toHaveLength(1);
		expect(badges[0].getAttribute('aria-label')).toMatch(/unknown/i);
	});

	it('shows a positive saving vs the best single-store baseline', () => {
		// This split totals €3.49; baseline best-single is €5.00 → saves €1.51.
		render(PlanCard, {
			plan: mockPlan({ stores: ['tesco', 'aldi'] }),
			title: 'Cheapest split',
			bestSingleTotalCents: 500,
			bestSingleItemIndices: [0, 1]
		});
		expect(screen.getByText(/Saves €1\.51/)).toBeTruthy();
	});

	it('hides a saving when the priced item set differs from the single-store baseline', () => {
		render(PlanCard, {
			plan: mockPlan({ lines: [mockPlan().lines[0]] }),
			title: 'Cheapest split',
			bestSingleTotalCents: 500,
			bestSingleItemIndices: [0, 1]
		});
		expect(screen.queryByText(/Saves|more than best single|Same price as best single/)).toBeNull();
	});

	it('shows the Dunnes voucher text inline when present', () => {
		const plan = mockPlan({
			stores: ['dunnes'],
			dunnes_voucher_offer_text: 'Spend €25 save €5'
		});
		render(PlanCard, { plan, title: 'Best single store' });
		expect(screen.getByText(/Spend €25 save €5/)).toBeTruthy();
	});

	it('labels the theoretical-minimum benchmark and suppresses its saving line', () => {
		render(PlanCard, {
			plan: mockPlan(),
			title: 'Theoretical minimum',
			benchmark: true,
			bestSingleTotalCents: 500
		});
		expect(screen.getByText(/benchmark/i)).toBeTruthy();
		// A benchmark is never routed, so no "saving vs" line is shown for it.
		expect(screen.queryByText(/Saves/)).toBeNull();
	});

	it('notes missing items have no known price here, never claiming unavailability', () => {
		render(PlanCard, { plan: mockPlan({ missing_count: 2 }), title: 'Best single store' });
		const missing = screen.getByText(/no known price here and are excluded from the total/);
		expect(missing.textContent).toMatch(/^2 items have no known price here/);
		// We can never prove a store lacks stock — only that we have no qualifying
		// price on record — so the copy must not claim unavailability.
		expect(missing.textContent).not.toMatch(/not available/i);
	});

	it('uses singular grammar for a single missing item', () => {
		render(PlanCard, { plan: mockPlan({ missing_count: 1 }), title: 'Best single store' });
		expect(
			screen.getByText('1 item has no known price here and is excluded from the total')
		).toBeTruthy();
	});

	it('renders the supplied reasons in order', () => {
		render(PlanCard, {
			plan: mockPlan(),
			title: 'Best single store',
			reasons: ['Cheapest overall', 'Only one shop needed']
		});
		const list = screen.getByRole('list', { name: 'Why this plan' });
		expect(within(list).getAllByRole('listitem').map((li) => li.textContent)).toEqual([
			'Cheapest overall',
			'Only one shop needed'
		]);
	});

	it('renders only the first four reasons when more are supplied', () => {
		render(PlanCard, {
			plan: mockPlan(),
			title: 'Best single store',
			reasons: ['One', 'Two', 'Three', 'Four', 'Five']
		});
		const list = screen.getByRole('list', { name: 'Why this plan' });
		expect(within(list).getAllByRole('listitem')).toHaveLength(4);
		expect(within(list).queryByText('Five')).toBeNull();
	});

	it('renders no reasons list when the prop is omitted or empty', () => {
		render(PlanCard, { plan: mockPlan(), title: 'Best single store' });
		expect(screen.queryByRole('list', { name: 'Why this plan' })).toBeNull();
	});
});
