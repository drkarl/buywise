// Client types + helpers for household receipt photos (issue #149).
//
// These mirror the wire shape of the receipts API: `GET
// /api/household/{id}/receipts` lists the household's receipts newest first,
// `POST /api/household/{id}/receipts?processing=local` uploads one image as raw
// bytes, the receipt's `image_url` serves the stored bytes, and `DELETE
// /api/household/{id}/receipts/{receipt_id}` removes one. The session cookie is
// sent automatically (same-origin); the upload and delete are CSRF-protected via
// `jsonMutationHeaders()`. The upload body is the image itself, so its
// `Content-Type` is the file's type rather than JSON.

import { jsonMutationHeaders } from '$lib/api/csrf';
import { HttpResponseError } from '$lib/api/household';

/** One household receipt, as `GET /api/household/{id}/receipts` returns it. */
export interface Receipt {
	/** `receipts.id`. */
	id: string;
	/** When the photo was uploaded (RFC 3339 UTC). */
	uploaded_at: string;
	/** When the stored photo is due to be purged (RFC 3339 UTC). */
	expires_at: string;
	/** The uploaded image's media type, e.g. `image/jpeg`. */
	content_type: string;
	/** The stored image size in bytes. */
	byte_size: number;
	/** Lowercase hex SHA-256 of the stored bytes. */
	sha256_hex: string;
	/** The user who uploaded it. */
	uploaded_by: string;
	/** How the photo is processed; only `local` is supported today. */
	processing: string;
	/** Absolute path serving the stored image bytes. */
	image_url: string;
}

/**
 * List the household's receipts from `GET /api/household/{id}/receipts`, newest
 * first. Throws on a non-OK response.
 */
export async function listReceipts(householdId: string): Promise<Receipt[]> {
	const res = await fetch(`/api/household/${encodeURIComponent(householdId)}/receipts`);
	if (!res.ok) {
		throw new HttpResponseError(res.status, `receipts failed (${res.status})`);
	}
	return (await res.json()) as Receipt[];
}

/** Absolute path serving one receipt's image bytes (for an `<img src>`). */
export function receiptImageUrl(householdId: string, receiptId: string): string {
	return `/api/household/${encodeURIComponent(householdId)}/receipts/${encodeURIComponent(receiptId)}`;
}

/**
 * Upload one image via `POST /api/household/{id}/receipts?processing=local`
 * (CSRF-protected). The file is the raw request body; `Content-Type` is the
 * file's own type (never JSON). Throws on a non-OK response — the caller maps
 * 413/415/409/501 to a specific message.
 */
export async function uploadReceipt(householdId: string, file: File): Promise<Receipt> {
	// Derive the CSRF header from the shared helper, then override the JSON
	// content type with the image's own type (the body is not JSON).
	const headers = {
		...jsonMutationHeaders(),
		'Content-Type': file.type || 'application/octet-stream'
	};
	const res = await fetch(
		`/api/household/${encodeURIComponent(householdId)}/receipts?processing=local`,
		{ method: 'POST', headers, body: file }
	);
	if (!res.ok) {
		throw new HttpResponseError(res.status, `upload failed (${res.status})`);
	}
	return (await res.json()) as Receipt;
}

/**
 * Delete one receipt via `DELETE /api/household/{id}/receipts/{receipt_id}`
 * (CSRF-protected). The server replies 204 and the caller drops it from the
 * list. Throws on a non-OK response.
 */
export async function deleteReceipt(householdId: string, receiptId: string): Promise<void> {
	const res = await fetch(receiptImageUrl(householdId, receiptId), {
		method: 'DELETE',
		headers: jsonMutationHeaders()
	});
	if (!res.ok) {
		throw new HttpResponseError(res.status, `delete receipt failed (${res.status})`);
	}
}
