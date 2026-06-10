import { afterEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';

import Page from './+page.svelte';

interface ListSummary {
	id: string;
	household_id: string;
	name: string;
	status: string;
	current_revision: number;
	created_at: string;
}

const USER = { id: 'u1', email: 'me@buywise.local', display_name: 'Me' };

function weeklyShop(): ListSummary {
	return {
		id: 'list-1',
		household_id: 'hh-1',
		name: 'Weekly Shop',
		status: 'active',
		current_revision: 0,
		created_at: '2026-06-01T10:00:00Z'
	};
}

interface StubOpts {
	/** Whether `GET /api/auth/me` and `GET /api/lists` see a live session initially. */
	authed?: boolean;
	/** Lists returned to an authenticated caller. */
	lists?: ListSummary[];
	/** Status for `POST /api/auth/login` (200 also flips the session to authed). */
	loginStatus?: number;
}

/**
 * Stub global fetch to model the auth gate: `/api/auth/me` and `/api/lists`
 * answer 401 until a successful login flips an internal `authed` flag, so the
 * login → list-index transition can be exercised end to end.
 */
function stub(opts: StubOpts = {}) {
	let authed = opts.authed ?? false;
	const lists = opts.lists ?? [];
	const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
		const url = typeof input === 'string' ? input : input.toString();
		const method = init?.method ?? 'GET';

		if (url.includes('/api/auth/me')) {
			return authed
				? new Response(JSON.stringify(USER), { status: 200 })
				: new Response('unauthorized', { status: 401 });
		}
		if (url.includes('/api/auth/login') && method === 'POST') {
			const status = opts.loginStatus ?? 200;
			if (status === 200) authed = true;
			return new Response(status === 200 ? JSON.stringify(USER) : 'no', { status });
		}
		if (url.includes('/api/auth/logout')) {
			authed = false;
			return new Response('', { status: 200 });
		}
		if (url.endsWith('/api/lists') && method === 'GET') {
			return authed
				? new Response(JSON.stringify(lists), { status: 200 })
				: new Response('unauthorized', { status: 401 });
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

describe('home page (auth gate + list index)', () => {
	it('shows the sign-in form when there is no session', async () => {
		stub({ authed: false });
		render(Page);
		expect(await screen.findByRole('heading', { name: 'Sign in' })).toBeTruthy();
	});

	it('shows the list index when already authenticated', async () => {
		stub({ authed: true, lists: [weeklyShop()] });
		render(Page);
		expect(await screen.findByText('Your lists')).toBeTruthy();
		expect(screen.getByText('Weekly Shop')).toBeTruthy();
		expect(screen.getByText(/Signed in as/)).toBeTruthy();
	});

	it('shows an empty state when the household has no lists', async () => {
		stub({ authed: true, lists: [] });
		render(Page);
		expect(await screen.findByText(/No lists yet/)).toBeTruthy();
	});

	it('surfaces an error on incorrect credentials', async () => {
		stub({ authed: false, loginStatus: 401 });
		render(Page);
		await screen.findByRole('heading', { name: 'Sign in' });

		await fireEvent.input(screen.getByLabelText('Email'), {
			target: { value: 'me@buywise.local' }
		});
		await fireEvent.input(screen.getByLabelText('Password'), {
			target: { value: 'wrong' }
		});
		await fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));

		expect(await screen.findByText('Incorrect email or password.')).toBeTruthy();
	});

	it('logs in and transitions to the list index', async () => {
		stub({ authed: false, lists: [weeklyShop()], loginStatus: 200 });
		render(Page);
		await screen.findByRole('heading', { name: 'Sign in' });

		await fireEvent.input(screen.getByLabelText('Email'), {
			target: { value: 'me@buywise.local' }
		});
		await fireEvent.input(screen.getByLabelText('Password'), {
			target: { value: 'buywise123' }
		});
		await fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));

		expect(await screen.findByText('Your lists')).toBeTruthy();
		expect(screen.getByText('Weekly Shop')).toBeTruthy();
	});
});
