import { afterEach, describe, expect, it, vi, type Mock } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import type { Settings } from '$lib/api/settings';

import Page from './+page.svelte';

/** A stubbed `fetch` (as installed by {@link stubSettings}). */
type FetchMock = Mock<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>;

/** A settings payload exercising all three statuses and the nearby flag. */
function baseSettings(): Settings {
	return {
		convenience_threshold_2_cents: 1000,
		convenience_threshold_3_cents: 2500,
		stores: [
			{ retailer: 'tesco', status: 'active', nearby: false },
			{ retailer: 'dunnes', status: 'occasional', nearby: false },
			{ retailer: 'supervalu', status: 'active', nearby: true },
			{ retailer: 'aldi', status: 'active', nearby: false },
			{ retailer: 'lidl', status: 'off', nearby: false }
		]
	};
}

/**
 * Stub global fetch: GET `/api/settings` answers with `settings`; a PATCH echoes
 * its own request body back (the endpoint returns the merged settings, and the
 * patch here carries every field, so echoing is equivalent for these tests).
 */
function stubSettings(settings: Settings): FetchMock {
	const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		if (!url.includes('/api/settings')) {
			return new Response('not found', { status: 404 });
		}
		if (init?.method === 'PATCH') {
			return new Response(init.body as string, { status: 200 });
		}
		return new Response(JSON.stringify(settings), { status: 200 });
	});
	vi.stubGlobal('fetch', fetchMock);
	return fetchMock;
}

/** The parsed body of the single PATCH request the page made. */
function patchBody(fetchMock: FetchMock) {
	const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'PATCH');
	if (!call) throw new Error('no PATCH request was made');
	return JSON.parse(call[1]!.body as string);
}

afterEach(() => {
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
});

describe('store settings', () => {
	it('renders a row per store with its status and nearby flag', async () => {
		stubSettings(baseSettings());
		render(Page);

		const dunnes = (await screen.findByLabelText('Dunnes status')) as HTMLSelectElement;
		expect(dunnes.value).toBe('occasional');
		expect((screen.getByLabelText('Lidl status') as HTMLSelectElement).value).toBe('off');
		expect((screen.getByLabelText('Tesco status') as HTMLSelectElement).value).toBe('active');
		expect((screen.getByLabelText('SuperValu is nearby') as HTMLInputElement).checked).toBe(true);
		expect((screen.getByLabelText('Tesco is nearby') as HTMLInputElement).checked).toBe(false);
	});

	it('saves both thresholds and every store in one PATCH', async () => {
		const fetchMock = stubSettings(baseSettings());
		render(Page);
		await screen.findByLabelText('Dunnes status');

		await fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));

		await waitFor(() => expect(fetchMock.mock.calls.some(([, i]) => i?.method === 'PATCH')).toBe(true));
		const body = patchBody(fetchMock);
		expect(body.convenience_threshold_2_cents).toBe(1000);
		expect(body.convenience_threshold_3_cents).toBe(2500);
		expect(body.stores).toHaveLength(5);
		expect(body.stores.find((s: { retailer: string }) => s.retailer === 'dunnes')).toEqual({
			retailer: 'dunnes',
			status: 'occasional',
			nearby: false
		});
		expect(body.stores.find((s: { retailer: string }) => s.retailer === 'supervalu')).toEqual({
			retailer: 'supervalu',
			status: 'active',
			nearby: true
		});
	});

	it('saves a changed store status', async () => {
		const fetchMock = stubSettings(baseSettings());
		render(Page);
		const dunnes = await screen.findByLabelText('Dunnes status');

		await fireEvent.change(dunnes, { target: { value: 'off' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));

		await waitFor(() => expect(fetchMock.mock.calls.some(([, i]) => i?.method === 'PATCH')).toBe(true));
		const body = patchBody(fetchMock);
		expect(
			body.stores.find((s: { retailer: string }) => s.retailer === 'dunnes').status
		).toBe('off');
	});
});
