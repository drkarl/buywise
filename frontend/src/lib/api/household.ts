// Client types + helpers for the member-facing household data controls
// (issue #136, roadmap #114).
//
// These mirror the wire shape of `crates/api/src/household.rs`: `GET
// /api/household` returns the caller's household, the export endpoint returns
// the documented JSON document as a download, and the delete endpoint takes the
// typed household name and the member's own password. The session cookie is sent
// automatically (same-origin); the delete is CSRF-protected via
// `jsonMutationHeaders()`.

import { jsonMutationHeaders } from '$lib/api/csrf';

/** The caller's household, as returned by `GET /api/household`. */
export interface Household {
	/** `households.id`. */
	id: string;
	/** `households.name` — the exact string the delete confirmation must repeat. */
	name: string;
}

/** An error carrying the HTTP status, so callers can react to a specific one. */
export class HttpResponseError extends Error {
	constructor(
		/** The response status code. */
		public readonly status: number,
		message: string
	) {
		super(message);
		this.name = 'HttpResponseError';
	}
}

/**
 * Fetch the caller's household from `GET /api/household`. Throws on a non-OK
 * response (404 when the caller belongs to no household).
 */
export async function fetchHousehold(): Promise<Household> {
	const res = await fetch('/api/household');
	if (!res.ok) {
		throw new HttpResponseError(res.status, `household failed (${res.status})`);
	}
	return (await res.json()) as Household;
}

/** Filename a browser saves a downloaded export under. */
export const EXPORT_FILENAME = 'buywise-household-export.json';

/**
 * Download the household's export from `GET /api/household/{id}/export` and
 * save it as a file. Throws on a non-OK response; the caller decides whether
 * that is an auth failure (401) or a generic error.
 */
export async function downloadHouseholdExport(householdId: string): Promise<void> {
	const res = await fetch(`/api/household/${encodeURIComponent(householdId)}/export`);
	if (!res.ok) {
		throw new HttpResponseError(res.status, `export failed (${res.status})`);
	}
	const blob = await res.blob();
	const url = URL.createObjectURL(blob);
	const link = document.createElement('a');
	link.href = url;
	link.download = EXPORT_FILENAME;
	document.body.appendChild(link);
	link.click();
	link.remove();
	URL.revokeObjectURL(url);
}

/**
 * Delete the household via `POST /api/household/{id}/delete` (CSRF-protected).
 * Requires the exact household name and the member's password; the server
 * refuses (422) if either does not match. On success the server also ends the
 * session, so the caller must leave for the sign-in page.
 */
export async function deleteHousehold(
	householdId: string,
	householdName: string,
	password: string
): Promise<void> {
	const res = await fetch(`/api/household/${encodeURIComponent(householdId)}/delete`, {
		method: 'POST',
		headers: jsonMutationHeaders(),
		body: JSON.stringify({ household_name: householdName, password })
	});
	if (!res.ok) {
		throw new HttpResponseError(res.status, `delete failed (${res.status})`);
	}
}

/**
 * Leave for the sign-in page after the session ends. The app has no client
 * router redirect; the home route renders the sign-in form when the session is
 * gone (see `routes/+page.svelte`).
 */
export function redirectToSignIn(): void {
	if (typeof window !== 'undefined') {
		window.location.assign('/');
	}
}
