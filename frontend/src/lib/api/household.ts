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
	/** The caller's role in this household. Owners alone may administer members. */
	role: MemberRole;
}

/** A household member's role (mirrors `household_members.role`). */
export type MemberRole = 'owner' | 'member';

/**
 * One household member as `GET /api/household/{id}/members` returns it. Never
 * carries a password hash or a session token.
 */
export interface Member {
	/** `users.id`. */
	user_id: string;
	/** `users.email`. */
	email: string;
	/** `users.display_name`. */
	display_name: string;
	/** `household_members.role`. */
	role: MemberRole;
	/** `household_members.joined_at` (RFC 3339 UTC). */
	joined_at: string;
}

/** The outcome of removing a member (mirrors `members::MemberRemoval`). */
export interface MemberRemoval {
	/** The removed user's id. */
	removed_user_id: string;
	/** How many of their session rows were deleted. */
	sessions_ended: number;
	/** Whether their account was deleted (only when they had no other household). */
	account_deleted: boolean;
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
 * List the household's members from `GET /api/household/{id}/members`
 * (owner-only; a plain member gets a 403). Throws on a non-OK response.
 */
export async function listMembers(householdId: string): Promise<Member[]> {
	const res = await fetch(`/api/household/${encodeURIComponent(householdId)}/members`);
	if (!res.ok) {
		throw new HttpResponseError(res.status, `members failed (${res.status})`);
	}
	return (await res.json()) as Member[];
}

/**
 * Add a member via `POST /api/household/{id}/members` (CSRF-protected,
 * owner-only). Takes the new member's email, display name and an initial
 * password the owner shares out of band; the password is not stored client-side
 * and the server never returns it. A duplicate email is a 409.
 */
export async function addMember(
	householdId: string,
	email: string,
	displayName: string,
	password: string
): Promise<Member> {
	const res = await fetch(`/api/household/${encodeURIComponent(householdId)}/members`, {
		method: 'POST',
		headers: jsonMutationHeaders(),
		body: JSON.stringify({ email, display_name: displayName, password })
	});
	if (!res.ok) {
		throw new HttpResponseError(res.status, `add member failed (${res.status})`);
	}
	return (await res.json()) as Member;
}

/**
 * Remove a member via `DELETE /api/household/{id}/members/{user_id}`
 * (CSRF-protected, owner-only). The server ends their sessions immediately,
 * removes their membership, and deletes their account only if they belong to no
 * other household; removing the last owner is a 409.
 */
export async function removeMember(
	householdId: string,
	userId: string
): Promise<MemberRemoval> {
	const res = await fetch(
		`/api/household/${encodeURIComponent(householdId)}/members/${encodeURIComponent(userId)}`,
		{ method: 'DELETE', headers: jsonMutationHeaders() }
	);
	if (!res.ok) {
		throw new HttpResponseError(res.status, `remove member failed (${res.status})`);
	}
	return (await res.json()) as MemberRemoval;
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
