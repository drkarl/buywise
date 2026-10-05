import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import PlanItem from './PlanItem.svelte';
import type { PlanLine } from '$lib/api/optimise';

/** A realistic priced plan line matching the wire contract. */
function mockLine(overrides: Partial<PlanLine> = {}): PlanLine {
	return {
		item_index: 0,
		entry_id: 'e-1',
		display_text: 'Milk 2L',
		product_name: 'Tesco Whole Milk 2L',
		canonical_item_id: 'ci-milk',
		retailer: 'tesco',
		retailer_product_id: 'rp-1',
		offer_id: 'of-1',
		unit_price_cents: 199,
		line_total_cents: 199,
		loyalty_applied: false,
		freshness_confidence: 'high',
		...overrides
	};
}

describe('PlanItem', () => {
	beforeEach(() => {
		// A JS-readable csrf cookie so the mutation helper stamps the header.
		document.cookie = 'csrf=tok-123';
	});

	afterEach(() => {
		vi.restoreAllMocks();
		document.cookie = 'csrf=; expires=' + new Date(0).toUTCString();
	});

	it('renders the line text, store, and price', () => {
		render(PlanItem, { line: mockLine() });
		expect(screen.getByText('Milk 2L')).toBeTruthy();
		expect(screen.getByText('Tesco')).toBeTruthy();
		expect(screen.getByText('€1.99')).toBeTruthy();
	});

	it('shows accept and reject buttons when the line identifies a match', () => {
		render(PlanItem, { line: mockLine() });
		expect(screen.getByRole('button', { name: /accept match/i })).toBeTruthy();
		expect(screen.getByRole('button', { name: /reject match/i })).toBeTruthy();
	});

	it('hides the feedback controls when canonical_item_id is null', () => {
		render(PlanItem, { line: mockLine({ canonical_item_id: null }) });
		expect(screen.queryByRole('button', { name: /accept match/i })).toBeNull();
		expect(screen.queryByRole('button', { name: /reject match/i })).toBeNull();
	});

	it('POSTs accept to /api/match-feedback with the CSRF header and the id pair', async () => {
		let capturedUrl = '';
		let capturedInit: RequestInit | undefined;
		const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({ status: 'accepted' }), { status: 201 })
		);

		render(PlanItem, { line: mockLine() });
		await fireEvent.click(screen.getByRole('button', { name: /accept match/i }));

		await waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());
		[capturedUrl, capturedInit] = fetchMock.mock.calls[0] as [string, RequestInit];
		expect(capturedUrl).toBe('/api/match-feedback');
		expect(capturedInit?.method).toBe('POST');
		expect(new Headers(capturedInit?.headers).get('X-CSRF-Token')).toBe('tok-123');
		const body = JSON.parse(capturedInit?.body as string);
		expect(body).toEqual({
			canonical_item_id: 'ci-milk',
			retailer_product_id: 'rp-1',
			action: 'accept'
		});

		// After a successful POST the line reflects the decision and hides buttons.
		await waitFor(() => expect(screen.getByText('Accepted')).toBeTruthy());
		expect(screen.queryByRole('button', { name: /accept match/i })).toBeNull();
	});

	it('POSTs reject and shows the rejected state', async () => {
		const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
			new Response(JSON.stringify({ status: 'rejected' }), { status: 201 })
		);

		render(PlanItem, { line: mockLine() });
		await fireEvent.click(screen.getByRole('button', { name: /reject match/i }));

		await waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());
		const body = JSON.parse((fetchMock.mock.calls[0][1] as RequestInit).body as string);
		expect(body.action).toBe('reject');
		await waitFor(() => expect(screen.getByText('Rejected')).toBeTruthy());
	});

	it('keeps the buttons and surfaces an error on a failed POST', async () => {
		vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response('nope', { status: 500 }));

		render(PlanItem, { line: mockLine() });
		await fireEvent.click(screen.getByRole('button', { name: /accept match/i }));

		await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy());
		// The buttons stay available so the user can retry.
		expect(screen.getByRole('button', { name: /accept match/i })).toBeTruthy();
	});
});
