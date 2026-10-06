import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import type { PurchaseDetail, PurchaseLineInput, PurchaseSummary } from '$lib/api/purchases';
import type { Receipt } from '$lib/api/receipts';

// Keep the real household API helpers (including HttpResponseError, whose
// identity the purchases module also uses), but replace the browser redirect
// with a spy so a 401 sign-out is observable in jsdom.
vi.mock('$lib/api/household', async (importOriginal) => {
	const actual = (await importOriginal()) as Record<string, unknown>;
	return { ...actual, redirectToSignIn: vi.fn() };
});

import Page from './+page.svelte';

const HOUSEHOLD = { id: 'hh-p', name: 'Our Household', role: 'owner' };
const BASE = `/api/household/${HOUSEHOLD.id}`;

/** The household the page reads for these tests. */
const SUMMARY_FIELDS = {
	household_id: HOUSEHOLD.id,
	currency: 'EUR',
	source_kind: 'manual',
	identity_kind: 'source_id',
	fingerprint: 'pl1:test',
	created_at: '2026-10-01T10:00:00.000Z',
	updated_at: '2026-10-01T10:00:00.000Z'
} as const;

function detail(overrides: Partial<PurchaseDetail> = {}): PurchaseDetail {
	const base: PurchaseDetail = {
		id: 'tx-1',
		retailer: 'tesco',
		purchase_date: '2026-10-01',
		source_transaction_id: 'uuid-1',
		total_cents: 437,
		revision: 1,
		line_count: 2,
		receipt_image_id: null,
		source_payload: '{}',
		lines: [
			{
				id: 'ln-1',
				transaction_id: 'tx-1',
				line_ordinal: 0,
				original_text: 'Milk 1L',
				name: null,
				brand: null,
				quantity: 2,
				size_value: null,
				size_unit: null,
				unit_price_cents: null,
				line_total_cents: 258,
				line_discount_cents: 0,
				deposit_cents: 0,
				retailer_product_id: null,
				canonical_item_id: null,
				match_status: 'unmatched',
				revision: 1,
				created_at: '2026-10-01T10:00:00.000Z'
			},
			{
				id: 'ln-2',
				transaction_id: 'tx-1',
				line_ordinal: 1,
				original_text: 'Bread',
				name: null,
				brand: null,
				quantity: 1,
				size_value: null,
				size_unit: null,
				unit_price_cents: null,
				line_total_cents: 199,
				line_discount_cents: 20,
				deposit_cents: 0,
				retailer_product_id: null,
				canonical_item_id: null,
				match_status: 'unmatched',
				revision: 1,
				created_at: '2026-10-01T10:00:00.000Z'
			}
		],
		revisions: [],
		...SUMMARY_FIELDS,
		...overrides
	};
	return base;
}

function summary(record: PurchaseDetail): PurchaseSummary {
	const { lines, revisions, ...rest } = record;
	void lines;
	void revisions;
	return rest;
}

function receipt(overrides: Partial<Receipt> = {}): Receipt {
	return {
		id: 'rc-1',
		uploaded_at: '2026-09-20T10:00:00Z',
		expires_at: '2026-10-20T10:00:00Z',
		content_type: 'image/jpeg',
		byte_size: 12345,
		sha256_hex: 'a'.repeat(64),
		uploaded_by: 'user-a',
		processing: 'local',
		image_url: `${BASE}/receipts/rc-1`,
		...overrides
	};
}

/** One recorded request, so tests can assert what the page actually sent. */
interface Recorder {
	method: string;
	url: string;
	body: unknown;
}

interface Store {
	purchases: PurchaseDetail[];
	receipts: Receipt[];
	requests: Recorder[];
}

function json(value: unknown, status = 200): Response {
	return new Response(JSON.stringify(value), { status });
}

/** Build the detail a manual save returns: the body's content at revision 1. */
function detailFromInput(id: string, input: PurchaseInputLike, revision = 1): PurchaseDetail {
	const lines = input.lines.map((line, index) => ({
		id: `${id}-ln-${index}`,
		transaction_id: id,
		line_ordinal: index,
		original_text: line.original_text,
		name: null,
		brand: null,
		quantity: line.quantity,
		size_value: null,
		size_unit: null,
		unit_price_cents: null,
		line_total_cents: line.line_total_cents,
		line_discount_cents: line.line_discount_cents,
		deposit_cents: 0,
		retailer_product_id: null,
		canonical_item_id: null,
		match_status: 'unmatched',
		revision,
		created_at: '2026-10-01T10:00:00.000Z'
	}));
	const sum = lines.reduce((acc, l) => acc + l.line_total_cents - l.line_discount_cents, 0);
	return detail({
		id,
		retailer: input.retailer.toLowerCase(),
		purchase_date: input.purchase_date,
		total_cents: input.total_cents ?? sum,
		line_count: lines.length,
		revision,
		receipt_image_id: input.receipt_image_id,
		source_payload: JSON.stringify(input),
		lines,
		source_transaction_id: `uuid-${id}`
	});
}

interface PurchaseInputLike {
	retailer: string;
	purchase_date: string;
	total_cents: number | null;
	receipt_image_id: string | null;
	lines: PurchaseLineInput[];
}

/**
 * Stub global fetch with a small in-memory ledger: `GET /api/household` answers
 * with {@link HOUSEHOLD}; the ledger list, one purchase, the manual create and
 * update, delete and the receipt-photo link, plus the households's receipt list
 * and an upload. Every request is recorded for assertions.
 */
function stubApi(store: Store, failListAfterInitialLoad = false) {
	let listCalls = 0;
	const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		const method = init?.method ?? 'GET';
		const rawBody = init?.body;
		store.requests.push({
			method,
			url,
			body: typeof rawBody === 'string' ? JSON.parse(rawBody) : rawBody
		});

		if (url === '/api/household') return json(HOUSEHOLD);
		if (url === `${BASE}/receipts` && method === 'GET') return json(store.receipts);
		if (url.includes('/receipts?processing=local')) {
			return json(receipt({ id: 'rc-new', image_url: `${BASE}/receipts/rc-new` }), 201);
		}
		if (url === `${BASE}/purchases` && method === 'GET') {
			listCalls += 1;
			if (failListAfterInitialLoad && listCalls > 1) throw new Error('network unavailable');
			return json(store.purchases.map(summary));
		}
		if (url === `${BASE}/purchases/manual` && method === 'POST') {
			const created = detailFromInput('tx-new', JSON.parse(String(rawBody)));
			store.purchases.push(created);
			return json(created, 201);
		}

		const link = url.match(/\/purchases\/([^/]+)\/receipt$/);
		if (link && method === 'PUT') {
			const record = store.purchases.find((p) => p.id === link[1]);
			if (!record) return new Response('missing', { status: 404 });
			const body = JSON.parse(String(rawBody)) as { receipt_image_id: string | null };
			record.receipt_image_id = body.receipt_image_id;
			return json(record);
		}

		const one = url.match(/\/purchases\/([^/]+)$/);
		if (one) {
			const index = store.purchases.findIndex((p) => p.id === one[1]);
			if (index === -1) return new Response('missing', { status: 404 });
			if (method === 'GET') return json(store.purchases[index]);
			if (method === 'PUT') {
				const input = JSON.parse(String(rawBody)) as PurchaseInputLike;
				const previous = store.purchases[index];
				const updated = detailFromInput(previous.id, input, previous.revision + 1);
				updated.revisions = [
					...previous.revisions,
					{
						revision: previous.revision,
						retailer: previous.retailer,
						purchase_date: previous.purchase_date,
						currency: previous.currency,
						total_cents: previous.total_cents,
						source_payload: '{}',
						superseded_at: '2026-10-02T10:00:00.000Z',
						lines: previous.lines
					}
				];
				store.purchases[index] = updated;
				return json(updated);
			}
			if (method === 'DELETE') {
				store.purchases.splice(index, 1);
				return json({ deleted: true });
			}
		}
		return new Response('not found', { status: 404 });
	});
	vi.stubGlobal('fetch', fetchMock);
	return fetchMock;
}

function makeStore(purchases: PurchaseDetail[] = [], receipts: Receipt[] = []): Store {
	return { purchases, receipts, requests: [] };
}

/** Type a value into a labelled input. */
async function typeInto(label: string | RegExp, value: string, index = 0) {
	const input = screen.getAllByLabelText(label)[index] as HTMLInputElement;
	await fireEvent.input(input, { target: { value } });
	return input;
}

beforeEach(() => {
	document.cookie = 'csrf=tok-123';
});

afterEach(() => {
	document.cookie = 'csrf=; Max-Age=0';
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
	vi.clearAllMocks();
});

describe('purchases', () => {
	it('lists the household purchases newest first with their total, lines and photo flag', async () => {
		stubApi(
			makeStore([
				detail({ id: 'tx-old', purchase_date: '2026-09-01', receipt_image_id: null }),
				detail({ id: 'tx-new', purchase_date: '2026-10-05', receipt_image_id: 'rc-1' })
			], [receipt()])
		);
		render(Page);

		const rows = await screen.findAllByRole('listitem');
		expect(rows[0].textContent).toContain('5 Oct 2026');
		expect(rows[0].textContent).toContain('€4.37');
		expect(rows[0].textContent).toContain('2 lines');
		expect(rows[0].textContent).toContain('Photo linked');
		expect(rows[1].textContent).toContain('1 Sep 2026');
		expect(rows[1].textContent).toContain('No photo');
	});

	it('adds a purchase with several lines, warning on a mismatch without blocking the save', async () => {
		const store = makeStore();
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Add purchase' }));

		await typeInto('Retailer', 'Tesco');
		await typeInto('Purchase date', '2026-10-03');
		await typeInto('Receipt total (€, optional)', '4.50');

		// First line, with the comma decimal separator an Irish keyboard types.
		await typeInto('Description', 'Milk 1L', 0);
		await typeInto('Quantity', '2', 0);
		await typeInto('Line total (€)', '2,58', 0);

		// Enter on the last line adds the next one, for keyboard entry.
		const lastTotal = screen.getAllByLabelText('Line total (€)')[0];
		await fireEvent.keyDown(lastTotal, { key: 'Enter' });

		await typeInto('Description', 'Bread', 1);
		await typeInto('Line total (€)', '1.99', 1);
		await typeInto('Discount (€, optional)', '0,20', 1);

		// The running sum is 2.58 + (1.99 − 0.20) = 4.37 against a 4.50 receipt.
		expect(screen.getByText('€4.37')).toBeTruthy();
		expect(
			screen.getByText(/The lines and the receipt total do not match/, { exact: false })
		).toBeTruthy();

		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));

		await waitFor(() =>
			expect(store.requests.some((r) => r.url.endsWith('/purchases/manual'))).toBe(true)
		);
		const call = store.requests.find((r) => r.url.endsWith('/purchases/manual'))!;
		expect(call.method).toBe('POST');
		expect(call.body).toEqual({
			retailer: 'Tesco',
			purchase_date: '2026-10-03',
			total_cents: 450,
			receipt_image_id: null,
			lines: [
				{
					original_text: 'Milk 1L',
					quantity: 2,
					line_total_cents: 258,
					line_discount_cents: 0
				},
				{
					original_text: 'Bread',
					quantity: 1,
					line_total_cents: 199,
					line_discount_cents: 20
				}
			]
		});

		// The save succeeded despite the mismatch: the new purchase's detail shows.
		expect(await screen.findByText('Entered by hand')).toBeTruthy();
	});

	it('edits a hand-entered purchase in place, keeping one purchase', async () => {
		const store = makeStore([detail()]);
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
		await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));

		await typeInto('Description', 'Sourdough', 1);
		await typeInto('Line total (€)', '2.42', 1);
		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));

		await waitFor(() => expect(store.requests.some((r) => r.method === 'PUT')).toBe(true));
		const call = store.requests.find((r) => r.method === 'PUT')!;
		expect(call.url).toBe(`${BASE}/purchases/tx-1`);
		const body = call.body as { lines: { original_text: string }[] };
		expect(body.lines[1].original_text).toBe('Sourdough');
		// One purchase, now at revision 2.
		expect(store.purchases).toHaveLength(1);
		expect(await screen.findByText('Revision 2')).toBeTruthy();
	});

	it('keeps a derived receipt total blank when editing and saves it as null', async () => {
		const store = makeStore([
			detail({
				total_cents: 437,
				source_payload: JSON.stringify({ total_cents: null }),
				lines: [
					{ ...detail().lines[0], line_total_cents: 500, line_discount_cents: 0 }
				],
				line_count: 1
			})
		]);
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
		await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
		expect((screen.getByLabelText('Receipt total (€, optional)') as HTMLInputElement).value).toBe('');
		expect(screen.queryByText(/The lines and the receipt total do not match/)).toBeNull();
		await typeInto('Line total (€)', '6.00');
		expect(screen.queryByText(/The lines and the receipt total do not match/)).toBeNull();
		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));

		await waitFor(() => expect(store.requests.some((r) => r.method === 'PUT')).toBe(true));
		const call = store.requests.find((r) => r.method === 'PUT')!;
		expect((call.body as { total_cents: number | null }).total_cents).toBeNull();
	});

	it('drops expired receipt ids from edits and hides unavailable detail photos', async () => {
		const store = makeStore([detail({ receipt_image_id: 'rc-expired' })]);
		stubApi(store);
		render(Page);

		const open = await screen.findByRole('button', { name: 'Open' });
		expect(open.closest('li')?.textContent).toContain('No photo');
		await fireEvent.click(open);
		expect(screen.queryByRole('img', { name: 'Linked receipt' })).toBeNull();
		expect(await screen.findByText(/No photo linked/)).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));

		await waitFor(() => expect(store.requests.some((r) => r.method === 'PUT')).toBe(true));
		const call = store.requests.find((r) => r.method === 'PUT')!;
		expect((call.body as { receipt_image_id: string | null }).receipt_image_id).toBeNull();
	});

	it('keeps the saved detail open when refreshing the list fails', async () => {
		const store = makeStore();
		const fetchMock = stubApi(store, true);
		render(Page);
		await fireEvent.click(await screen.findByRole('button', { name: 'Add purchase' }));
		await typeInto('Retailer', 'Tesco');
		await typeInto('Description', 'Milk');
		await typeInto('Line total (€)', '1.00');
		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));

		expect(await screen.findByText('Purchase saved, but the list could not be refreshed. It may be out of date.')).toBeTruthy();
		expect(await screen.findByText('Entered by hand')).toBeTruthy();
		expect(fetchMock.mock.calls.filter(([input, init]) => {
			const url = typeof input === 'string' ? input : input.toString();
			return url.endsWith('/purchases/manual') && init?.method === 'POST';
		})).toHaveLength(1);
	});

	it('shows the deleted list state when refreshing after deletion fails', async () => {
		const store = makeStore([detail()]);
		stubApi(store, true);
		render(Page);
		await fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
		await fireEvent.click(await screen.findByRole('button', { name: 'Delete' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Confirm delete' }));

		expect(await screen.findByText('Purchase deleted, but the list could not be refreshed. It may be out of date.')).toBeTruthy();
		expect(await screen.findByText(/No purchases yet/)).toBeTruthy();
		expect(screen.queryByText('Couldn’t delete the purchase. Check your connection and try again.')).toBeNull();
	});

	it('keeps an unlinked photo cleared when refreshing the list fails', async () => {
		const store = makeStore([detail({ receipt_image_id: 'rc-1' })], [receipt()]);
		stubApi(store, true);
		render(Page);
		await fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
		await fireEvent.click(await screen.findByRole('button', { name: 'Remove photo' }));

		expect(await screen.findByText('Photo link cleared, but the list could not be refreshed. It may be out of date.')).toBeTruthy();
		expect(await screen.findByText(/No photo linked/)).toBeTruthy();
		expect(screen.queryByText('Couldn’t remove the photo link. Please try again.')).toBeNull();
		expect(store.purchases[0].receipt_image_id).toBeNull();
	});

	it('deletes a purchase only after the inline confirmation', async () => {
		const store = makeStore([detail()]);
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
		await fireEvent.click(await screen.findByRole('button', { name: 'Delete' }));

		expect(store.requests.some((r) => r.method === 'DELETE')).toBe(false);
		await fireEvent.click(screen.getByRole('button', { name: 'Confirm delete' }));

		await waitFor(() => expect(store.requests.some((r) => r.method === 'DELETE')).toBe(true));
		expect(store.requests.find((r) => r.method === 'DELETE')!.url).toBe(
			`${BASE}/purchases/tx-1`
		);
		expect(store.purchases).toHaveLength(0);
		expect(await screen.findByText('No purchases yet. Add one by hand and it will appear here, ready to match against your receipt photo.')).toBeTruthy();
	});

	it('links a chosen household photo to the purchase being saved', async () => {
		const store = makeStore([], [receipt(), receipt({ id: 'rc-2' })]);
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Add purchase' }));
		await typeInto('Retailer', 'Lidl');
		await typeInto('Description', 'Milk', 0);
		await typeInto('Line total (€)', '1.00', 0);

		// The photo panel is present as a switchable tab.
		await fireEvent.click(screen.getByRole('tab', { name: 'Receipt photo' }));
		await fireEvent.click(screen.getAllByRole('button', { name: /Receipt from/ })[1]);

		// Zoom toggles on the linked photo.
		await fireEvent.click(screen.getByRole('button', { name: 'Zoom in' }));

		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));
		await waitFor(() =>
			expect(store.requests.some((r) => r.url.endsWith('/purchases/manual'))).toBe(true)
		);
		const call = store.requests.find((r) => r.url.endsWith('/purchases/manual'))!;
		expect((call.body as { receipt_image_id: string | null }).receipt_image_id).toBe('rc-2');
	});

	it('uploads a photo from the form and links the uploaded image', async () => {
		const store = makeStore();
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Add purchase' }));
		const input = (await screen.findByLabelText('Receipt photo file')) as HTMLInputElement;
		const file = new File([new Uint8Array([9, 8, 7])], 'shop.png', { type: 'image/png' });
		Object.defineProperty(input, 'files', { value: [file], configurable: true });
		await fireEvent.change(input);
		await fireEvent.click(screen.getByRole('button', { name: 'Upload and link' }));

		await waitFor(() =>
			expect(store.requests.some((r) => r.url.includes('/receipts?processing=local'))).toBe(true)
		);
		const upload = store.requests.find((r) => r.url.includes('/receipts?processing=local'))!;
		expect(upload.method).toBe('POST');
		expect(upload.body).toBe(file);

		// The uploaded photo is now the linked one.
		await typeInto('Retailer', 'Aldi');
		await typeInto('Description', 'Bananas', 0);
		await typeInto('Line total (€)', '1.20', 0);
		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));
		await waitFor(() =>
			expect(store.requests.some((r) => r.url.endsWith('/purchases/manual'))).toBe(true)
		);
		const call = store.requests.find((r) => r.url.endsWith('/purchases/manual'))!;
		expect((call.body as { receipt_image_id: string | null }).receipt_image_id).toBe('rc-new');
	});

	it('clears a purchase’s photo link from its detail view', async () => {
		const store = makeStore([detail({ receipt_image_id: 'rc-1' })], [receipt()]);
		stubApi(store);
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
		await fireEvent.click(await screen.findByRole('button', { name: 'Remove photo' }));

		await waitFor(() =>
			expect(
				store.requests.some(
					(r) => r.method === 'PUT' && r.url.endsWith('/purchases/tx-1/receipt')
				)
			).toBe(true)
		);
		const call = store.requests.find((r) => r.url.endsWith('/purchases/tx-1/receipt'))!;
		expect(call.body).toEqual({ receipt_image_id: null });
		expect(store.purchases[0].receipt_image_id).toBeNull();
	});

	it('shows a specific message when the server refuses the amounts', async () => {
		const store = makeStore();
		stubApi(store);
		// Make the create answer the documented 422 shape.
		const fetchMock = vi.mocked(globalThis.fetch);
		fetchMock.mockImplementation(async (input: RequestInfo | URL) => {
			const url = typeof input === 'string' ? input : input.toString();
			if (url.endsWith('/purchases/manual')) {
				return new Response(
					JSON.stringify({ errors: ['lines[0].line_total_cents: must not be negative'] }),
					{ status: 422 }
				);
			}
			if (url === '/api/household') return json(HOUSEHOLD);
			return json([]);
		});
		render(Page);

		await fireEvent.click(await screen.findByRole('button', { name: 'Add purchase' }));
		await typeInto('Retailer', 'Spar');
		await typeInto('Description', 'Milk', 0);
		await typeInto('Line total (€)', '1.00', 0);
		await fireEvent.click(screen.getByRole('button', { name: 'Save purchase' }));

		expect(
			await screen.findByText('lines[0].line_total_cents: must not be negative')
		).toBeTruthy();
	});
});
