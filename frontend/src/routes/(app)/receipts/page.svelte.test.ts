import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import type { Receipt } from '$lib/api/receipts';

// Keep the real household API helpers (including HttpResponseError, whose
// identity the receipts module also uses), but replace the browser redirect with
// a spy so a 401 sign-out is observable in jsdom.
vi.mock('$lib/api/household', async (importOriginal) => {
	const actual = (await importOriginal()) as Record<string, unknown>;
	return { ...actual, redirectToSignIn: vi.fn() };
});

import { redirectToSignIn } from '$lib/api/household';
import Page from './+page.svelte';

/** The household `GET /api/household` reports for these tests. */
const HOUSEHOLD = { id: 'hh-receipts', name: 'Our Household', role: 'owner' };

/** One receipt, as `GET /api/household/{id}/receipts` returns it. */
function baseReceipt(overrides: Partial<Receipt> = {}): Receipt {
	return {
		id: 'rc-1',
		uploaded_at: '2026-09-20T10:00:00Z',
		expires_at: '2026-10-20T10:00:00Z',
		content_type: 'image/jpeg',
		byte_size: 12345,
		sha256_hex: 'a'.repeat(64),
		uploaded_by: 'user-owner',
		processing: 'local',
		image_url: '/api/household/hh-receipts/receipts/rc-1',
		...overrides
	};
}

/** A stubbed `fetch` (as installed by {@link stubReceipts}). */
type FetchMock = Mock<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>;

/**
 * Stub global fetch: `GET /api/household` answers with {@link HOUSEHOLD}; the
 * receipt list answers with `receipts`; the `?processing=local` upload answers
 * with a fresh receipt (or `options.uploadStatus`); the per-receipt endpoint
 * answers 204 for a DELETE (or `options.deleteStatus`).
 */
function stubReceipts(
	receipts: Receipt[],
	options: { uploadStatus?: number; deleteStatus?: number } = {}
): FetchMock {
	const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		if (url === '/api/household') {
			return new Response(JSON.stringify(HOUSEHOLD), { status: 200 });
		}
		if (url.includes('/receipts?processing=local')) {
			const status = options.uploadStatus ?? 201;
			if (status !== 201) return new Response('refused', { status });
			return new Response(
				JSON.stringify(
					baseReceipt({
						id: 'rc-new',
						uploaded_at: '2026-10-01T10:00:00Z',
						expires_at: '2026-10-31T10:00:00Z',
						content_type: 'image/png',
						image_url: '/api/household/hh-receipts/receipts/rc-new'
					})
				),
				{ status }
			);
		}
		if (url.endsWith('/receipts')) {
			return new Response(JSON.stringify(receipts), { status: 200 });
		}
		if (url.includes('/receipts/')) {
			const status = options.deleteStatus ?? 204;
			return new Response(status === 204 ? null : 'refused', { status });
		}
		return new Response('not found', { status: 404 });
	});
	vi.stubGlobal('fetch', fetchMock);
	return fetchMock;
}

/**
 * Choose a file on a file input. `@testing-library/dom` in this version has no
 * `fireEvent.upload`, so set `files` directly and dispatch the change event the
 * page listens for.
 */
async function choosePhoto(input: HTMLInputElement, file: File) {
	Object.defineProperty(input, 'files', { value: [file], configurable: true });
	await fireEvent.change(input);
}

beforeEach(() => {
	// The upload and delete are mutating requests; the page reads the CSRF token
	// from this cookie.
	document.cookie = 'csrf=tok-123';
});

afterEach(() => {
	document.cookie = 'csrf=; Max-Age=0';
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
	vi.clearAllMocks();
});

describe('receipts', () => {
	it('lists receipts with their upload and expiry dates', async () => {
		stubReceipts([
			baseReceipt(),
			baseReceipt({
				id: 'rc-2',
				uploaded_at: '2026-09-01T10:00:00Z',
				expires_at: '2026-10-01T10:00:00Z'
			})
		]);
		render(Page);

		expect(await screen.findByText('Expires 20 Oct 2026')).toBeTruthy();
		expect(screen.getByText('Expires 1 Oct 2026')).toBeTruthy();
		expect(screen.getByText(/^20 Sep/)).toBeTruthy();
		expect(screen.getByText(/^1 Sep/)).toBeTruthy();
	});

	it('explains receipt storage and that reading is not available yet', async () => {
		stubReceipts([]);
		render(Page);

		expect(
			await screen.findByText(
				'Photos are stored privately on this household’s own server and deleted after 30 days. Reading them is not available yet.'
			)
		).toBeTruthy();
		expect((screen.getByLabelText('Receipt photo') as HTMLInputElement).hasAttribute('capture')).toBe(
			false
		);
	});

	it('shows an empty state when the household has no receipts', async () => {
		stubReceipts([]);
		render(Page);

		expect(await screen.findByText(/No receipts yet/)).toBeTruthy();
	});

	it('rejects an unsupported image type with a specific message', async () => {
		stubReceipts([], { uploadStatus: 415 });
		render(Page);

		const input = (await screen.findByLabelText('Receipt photo')) as HTMLInputElement;
		const file = new File([new Uint8Array([1, 2, 3])], 'notes.gif', { type: 'image/gif' });
		await choosePhoto(input, file);
		await fireEvent.click(screen.getByRole('button', { name: 'Upload' }));

		expect(
			await screen.findByText('Only JPEG, PNG, WebP or HEIC photos are accepted.')
		).toBeTruthy();
	});

	it('uploads the raw file with its own content type and the CSRF header, then shows it', async () => {
		const fetchMock = stubReceipts([baseReceipt()]);
		render(Page);
		await screen.findByText('Expires 20 Oct 2026');

		const file = new File([new Uint8Array([9, 8, 7])], 'shop.png', { type: 'image/png' });
		await choosePhoto(screen.getByLabelText('Receipt photo') as HTMLInputElement, file);

		// The Upload button is enabled only once a file is chosen.
		const uploadButton = screen.getByRole('button', { name: 'Upload' }) as HTMLButtonElement;
		expect(uploadButton.disabled).toBe(false);
		await fireEvent.click(uploadButton);

		await waitFor(() =>
			expect(
				fetchMock.mock.calls.some(([url]) =>
					String(url).includes('/receipts?processing=local')
				)
			).toBe(true)
		);
		const call = fetchMock.mock.calls.find(([url]) =>
			String(url).includes('/receipts?processing=local')
		);
		expect(call![0]).toBe('/api/household/hh-receipts/receipts?processing=local');
		expect(call![1]!.method).toBe('POST');
		// The body is the file itself, not JSON.
		expect(call![1]!.body).toBe(file);
		const headers = new Headers(call![1]!.headers);
		expect(headers.get('Content-Type')).toBe('image/png');
		expect(headers.get('Content-Type')).not.toContain('application/json');
		expect(headers.get('X-CSRF-Token')).toBe('tok-123');

		// The new receipt is prepended and the input is cleared.
		expect(await screen.findByText('Expires 31 Oct 2026')).toBeTruthy();
		expect((screen.getByLabelText('Receipt photo') as HTMLInputElement).value).toBe('');
	});

	it('confirms before deleting and only then issues the DELETE', async () => {
		const fetchMock = stubReceipts([baseReceipt()]);
		render(Page);
		await screen.findByText('Expires 20 Oct 2026');

		await fireEvent.click(screen.getByRole('button', { name: 'Remove' }));

		expect(await screen.findByText('Delete this receipt? It cannot be undone.')).toBeTruthy();
		expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(false);

		await fireEvent.click(screen.getByRole('button', { name: 'Confirm delete' }));

		await waitFor(() =>
			expect(
				fetchMock.mock.calls.some(
					([url, init]) =>
						String(url) === '/api/household/hh-receipts/receipts/rc-1' &&
						init?.method === 'DELETE'
				)
			).toBe(true)
		);
		const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'DELETE');
		expect(new Headers(call![1]!.headers).get('X-CSRF-Token')).toBe('tok-123');
		await waitFor(() => expect(screen.queryByText('Expires 20 Oct 2026')).toBeNull());
	});
});
