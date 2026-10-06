import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import type { Household, Member } from '$lib/api/household';
import type { Settings } from '$lib/api/settings';

// Keep the real household API helpers, but replace the browser redirect with a
// spy so the post-delete sign-out is observable in jsdom.
vi.mock('$lib/api/household', async (importOriginal) => {
	const actual = (await importOriginal()) as Record<string, unknown>;
	return { ...actual, redirectToSignIn: vi.fn() };
});

import { redirectToSignIn } from '$lib/api/household';
import Page from './+page.svelte';

/** The household `GET /api/household` reports for these tests. */
const HOUSEHOLD: Household = { id: 'hh-settings', name: 'Our Household', role: 'owner' };

/** Two members: an owner and a plain member, as `GET …/members` returns them. */
function baseMembers(): Member[] {
	return [
		{
			user_id: 'user-owner',
			email: 'owner@example.test',
			display_name: 'Olive',
			role: 'owner',
			joined_at: '2026-01-01T00:00:00Z'
		},
		{
			user_id: 'user-2',
			email: 'member@example.test',
			display_name: 'Robin',
			role: 'member',
			joined_at: '2026-02-01T00:00:00Z'
		}
	];
}

/** A stubbed `fetch` (as installed by {@link stubSettings}). */
type FetchMock = Mock<(input: RequestInfo | URL, init?: RequestInit) => Promise<Response>>;

/** A settings payload exercising all three statuses, the nearby flag, and rules. */
function baseSettings(): Settings {
	return {
		convenience_threshold_2_cents: 1000,
		convenience_threshold_3_cents: 2500,
		categories: [
			{ id: 'cat-dairy', name_en: 'Dairy', name_es: 'Lácteos', icon: 'milk', default_order: 1 },
			{ id: 'cat-pantry', name_en: 'Pantry', name_es: 'Despensa', icon: 'package', default_order: 2 }
		],
		subcategories: [
			{
				id: 'sub-breakfast',
				category_id: 'cat-pantry',
				name_en: 'Breakfast',
				name_es: 'Desayuno',
				default_order: 1
			}
		],
		stores: [
			{ retailer: 'tesco', status: 'active', nearby: false, category_rules: [] },
			{ retailer: 'dunnes', status: 'occasional', nearby: false, category_rules: [] },
			{ retailer: 'supervalu', status: 'active', nearby: true, category_rules: [] },
			{
				retailer: 'aldi',
				status: 'active',
				nearby: false,
				category_rules: [{ category_id: 'cat-dairy', subcategory_id: null, allowed: false }]
			},
			{ retailer: 'lidl', status: 'off', nearby: false, category_rules: [] }
		]
	};
}

/**
 * Stub global fetch: GET `/api/settings` answers with `settings`; a PATCH echoes
 * its own request body back (the endpoint returns the merged settings, and the
 * patch here carries every field, so echoing is equivalent for these tests).
 * `GET /api/household` answers with {@link HOUSEHOLD} (role overridable); the
 * member endpoints answer from `options`; the household export and delete
 * endpoints answer with `options`, defaulting to success.
 */
function stubSettings(
	settings: Settings,
	options: {
		deleteStatus?: number;
		exportStatus?: number;
		role?: Household['role'];
		members?: Member[];
		addStatus?: number;
		removeStatus?: number;
	} = {}
): FetchMock {
	const role = options.role ?? 'owner';
	const members = options.members ?? [];
	const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		if (url.includes('/api/household')) {
			if (url.endsWith('/delete')) {
				const status = options.deleteStatus ?? 200;
				return new Response(status === 200 ? JSON.stringify({ removed_rows: 3 }) : 'refused', {
					status
				});
			}
			if (url.endsWith('/export')) {
				const status = options.exportStatus ?? 200;
				return new Response(status === 200 ? '{"format":"buywise-household-export"}' : 'failed', {
					status
				});
			}
			if (url.includes('/members/')) {
				const status = options.removeStatus ?? 200;
				return new Response(
					status === 200
						? JSON.stringify({
								removed_user_id: 'user-2',
								sessions_ended: 1,
								account_deleted: true,
								account_deactivated: false
							})
						: 'refused',
					{ status }
				);
			}
			if (url.endsWith('/members')) {
				if (init?.method === 'POST') {
					const status = options.addStatus ?? 201;
					if (status !== 201) return new Response('refused', { status });
					const body = JSON.parse(init.body as string);
					return new Response(
						JSON.stringify({
							user_id: 'user-new',
							email: body.email,
							display_name: body.display_name,
							role: 'member',
							joined_at: '2026-03-01T00:00:00Z'
						}),
						{ status }
					);
				}
				return new Response(JSON.stringify(members), { status: 200 });
			}
			return new Response(JSON.stringify({ id: HOUSEHOLD.id, name: HOUSEHOLD.name, role }), {
				status: 200
			});
		}
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

beforeEach(() => {
	// The delete is a mutating request; the page reads the CSRF token from this
	// cookie (mirrors the convention used by the component tests).
	document.cookie = 'csrf=tok-123';
});

afterEach(() => {
	document.cookie = 'csrf=; Max-Age=0';
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
	vi.clearAllMocks();
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
			nearby: false,
			category_rules: []
		});
		expect(body.stores.find((s: { retailer: string }) => s.retailer === 'supervalu')).toEqual({
			retailer: 'supervalu',
			status: 'active',
			nearby: true,
			category_rules: []
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

	it('reflects stored category rules as unchecked checkboxes', async () => {
		stubSettings(baseSettings());
		render(Page);
		await screen.findByLabelText('Aldi status');

		// Aldi's stored rule excludes Dairy; every other category stays allowed.
		expect((screen.getByLabelText('Aldi allows Dairy') as HTMLInputElement).checked).toBe(false);
		expect((screen.getByLabelText('Aldi allows Pantry') as HTMLInputElement).checked).toBe(true);
		expect((screen.getByLabelText('Tesco allows Dairy') as HTMLInputElement).checked).toBe(true);
	});

	it('saves a category exclusion when a category is unticked', async () => {
		const fetchMock = stubSettings(baseSettings());
		render(Page);
		await screen.findByLabelText('Tesco status');

		await fireEvent.click(screen.getByLabelText('Tesco allows Dairy'));
		await fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));

		await waitFor(() => expect(fetchMock.mock.calls.some(([, i]) => i?.method === 'PATCH')).toBe(true));
		const body = patchBody(fetchMock);
		expect(body.stores.find((s: { retailer: string }) => s.retailer === 'tesco')).toEqual({
			retailer: 'tesco',
			status: 'active',
			nearby: false,
			category_rules: [{ category_id: 'cat-dairy', subcategory_id: null, allowed: false }]
		});
	});

	it('saves a sub-category override when a sub-category is re-ticked', async () => {
		const fetchMock = stubSettings(baseSettings());
		render(Page);
		await screen.findByLabelText('Tesco status');

		// Exclude the whole Pantry category, then re-allow its Breakfast split.
		await fireEvent.click(screen.getByLabelText('Tesco allows Pantry'));
		await fireEvent.click(screen.getByLabelText('Tesco allows Breakfast'));
		await fireEvent.click(screen.getByRole('button', { name: 'Save settings' }));

		await waitFor(() => expect(fetchMock.mock.calls.some(([, i]) => i?.method === 'PATCH')).toBe(true));
		const body = patchBody(fetchMock);
		expect(
			body.stores.find((s: { retailer: string }) => s.retailer === 'tesco').category_rules
		).toEqual([
			{ category_id: 'cat-pantry', subcategory_id: null, allowed: false },
			{ category_id: 'cat-pantry', subcategory_id: 'sub-breakfast', allowed: true }
		]);
	});
});

describe('household data controls', () => {
	/** Fill the guarded delete form. */
	async function fillDeleteForm(name: string, password: string) {
		await fireEvent.input(screen.getByLabelText(/type the household name/i), {
			target: { value: name }
		});
		await fireEvent.input(screen.getByLabelText('Your password'), {
			target: { value: password }
		});
	}

	it('disables delete until the exact household name and a password are given', async () => {
		stubSettings(baseSettings());
		render(Page);
		const button = (await screen.findByRole('button', {
			name: 'Delete household data'
		})) as HTMLButtonElement;
		expect(button.disabled).toBe(true);

		await fillDeleteForm('Not our household', 'secret');
		expect(button.disabled).toBe(true);

		await fillDeleteForm(HOUSEHOLD.name, '');
		expect(button.disabled).toBe(true);

		await fillDeleteForm(HOUSEHOLD.name, 'secret');
		expect(button.disabled).toBe(false);
	});

	it('deletes with the exact name and password, then returns to the sign-in page', async () => {
		const fetchMock = stubSettings(baseSettings());
		render(Page);
		await screen.findByText(/Download everything BuyWise holds/);
		expect(
			screen.getByText(
				/Member accounts left with no other household are signed out and deleted, or deactivated/
			)
		).toBeTruthy();

		await fillDeleteForm(HOUSEHOLD.name, 'secret');
		await fireEvent.click(screen.getByRole('button', { name: 'Delete household data' }));

		await waitFor(() => expect(redirectToSignIn).toHaveBeenCalledTimes(1));

		const call = fetchMock.mock.calls.find(([url]) => String(url).endsWith('/delete'));
		expect(call).toBeTruthy();
		expect(JSON.parse(call![1]!.body as string)).toEqual({
			household_name: HOUSEHOLD.name,
			password: 'secret'
		});
		expect(new Headers(call![1]!.headers).get('X-CSRF-Token')).toBe('tok-123');
	});

	it('shows an error and does not sign out when the server refuses the delete', async () => {
		stubSettings(baseSettings(), { deleteStatus: 422 });
		render(Page);
		await screen.findByText(/Download everything BuyWise holds/);

		await fillDeleteForm(HOUSEHOLD.name, 'wrong-password');
		await fireEvent.click(screen.getByRole('button', { name: 'Delete household data' }));

		expect(await screen.findByText(/didn’t match/)).toBeTruthy();
		expect(redirectToSignIn).not.toHaveBeenCalled();
	});

	it('shows an error when the export download fails', async () => {
		stubSettings(baseSettings(), { exportStatus: 500 });
		render(Page);
		await fireEvent.click(await screen.findByRole('button', { name: 'Download export' }));
		expect(await screen.findByText(/Couldn’t download your export/)).toBeTruthy();
	});
});

describe('household members', () => {
	/** Fill the add-member form. */
	async function fillAddForm(name: string, email: string, password: string) {
		await fireEvent.input(screen.getByLabelText('Display name'), { target: { value: name } });
		await fireEvent.input(screen.getByLabelText('Email'), { target: { value: email } });
		await fireEvent.input(screen.getByLabelText('Initial password'), {
			target: { value: password }
		});
	}

	it('lists the members with their roles for an owner', async () => {
		stubSettings(baseSettings(), { members: baseMembers() });
		render(Page);

		expect(await screen.findByRole('heading', { name: 'Household members' })).toBeTruthy();
		expect(await screen.findByText('Olive')).toBeTruthy();
		expect(screen.getByText('owner@example.test')).toBeTruthy();
		expect(screen.getByText('Robin')).toBeTruthy();
		expect(screen.getByText('Owner')).toBeTruthy();
		expect(screen.getByText('Member')).toBeTruthy();
	});

	it('adds a member with the initial password supplied to the server', async () => {
		const fetchMock = stubSettings(baseSettings(), { members: baseMembers() });
		render(Page);
		await screen.findByLabelText('Display name');

		await fillAddForm('Sam', 'sam@example.test', 'temp-password');
		await fireEvent.click(screen.getByRole('button', { name: 'Add member' }));

		await waitFor(() =>
			expect(
				fetchMock.mock.calls.some(
					([url, init]) => String(url).endsWith('/members') && init?.method === 'POST'
				)
			).toBe(true)
		);
		const call = fetchMock.mock.calls.find(
			([url, init]) => String(url).endsWith('/members') && init?.method === 'POST'
		);
		expect(JSON.parse(call![1]!.body as string)).toEqual({
			email: 'sam@example.test',
			display_name: 'Sam',
			password: 'temp-password'
		});
		expect(new Headers(call![1]!.headers).get('X-CSRF-Token')).toBe('tok-123');

		// The new member appears, the password field is cleared, and the message
		// tells the owner the password is not shown again.
		expect(await screen.findByText('Sam')).toBeTruthy();
		expect((screen.getByLabelText('Initial password') as HTMLInputElement).value).toBe('');
		expect(await screen.findByText(/Share the password with them directly/)).toBeTruthy();
	});

	it('shows a clear message when the email is already taken', async () => {
		stubSettings(baseSettings(), { members: baseMembers(), addStatus: 409 });
		render(Page);
		await screen.findByLabelText('Display name');

		await fillAddForm('Sam', 'member@example.test', 'temp-password');
		await fireEvent.click(screen.getByRole('button', { name: 'Add member' }));

		expect(await screen.findByText(/already exists/)).toBeTruthy();
	});

	it('confirms before removing a member, stating what happens, and sends no delete until confirmed', async () => {
		const fetchMock = stubSettings(baseSettings(), { members: baseMembers() });
		render(Page);
		await screen.findByText('Robin');

		await fireEvent.click(screen.getByRole('button', { name: 'Remove' }));

		expect(await screen.findByText(/signed out immediately/)).toBeTruthy();
		expect(
			screen.getByText(/deactivated and shown as a former member when/)
		).toBeTruthy();
		expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'DELETE')).toBe(false);

		await fireEvent.click(screen.getByRole('button', { name: 'Confirm remove' }));

		await waitFor(() =>
			expect(
				fetchMock.mock.calls.some(
					([url, init]) => String(url).includes('/members/user-2') && init?.method === 'DELETE'
				)
			).toBe(true)
		);
		await waitFor(() => expect(screen.queryByText('Robin')).toBeNull());
	});

	it('withholds the remove control from the last owner', async () => {
		stubSettings(baseSettings(), { members: [baseMembers()[0]] });
		render(Page);
		await screen.findByText('Olive');

		expect(screen.getByText('Last owner')).toBeTruthy();
		expect(screen.queryByRole('button', { name: 'Remove' })).toBeNull();
	});

	it('hides the members section and the delete action from a plain member', async () => {
		stubSettings(baseSettings(), { role: 'member', members: baseMembers() });
		render(Page);
		await screen.findByText(/Download everything BuyWise holds/);

		expect(screen.queryByRole('heading', { name: 'Household members' })).toBeNull();
		expect(screen.queryByRole('button', { name: 'Delete household data' })).toBeNull();
		expect(screen.queryByText(/or delete it/)).toBeNull();
		// Export stays available to every member.
		expect(screen.getByRole('button', { name: 'Download export' })).toBeTruthy();
	});
});
