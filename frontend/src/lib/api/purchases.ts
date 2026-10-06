// Client types + helpers for the household purchase ledger (issue #148) and the
// hand-entered purchases and receipt links added in issue #156.
//
// These mirror the wire shape of `crates/api/src/purchases.rs`: `GET
// /api/household/{id}/purchases` lists the household's transactions (no lines),
// `GET /api/household/{id}/purchases/{txid}` returns one with its lines and
// superseded revisions, `POST .../purchases/manual` adds one typed by hand,
// `PUT .../purchases/{txid}` corrects a hand-entered one (a new revision of the
// same purchase, never a second one), `PUT .../purchases/{txid}/receipt` sets or
// clears its receipt photo, and `DELETE .../purchases/{txid}` removes it. The
// session cookie is sent automatically (same-origin); every mutation carries the
// CSRF token via `jsonMutationHeaders()`.
//
// Money is integer cents on the wire; `$lib/purchases/money` converts the euro
// amounts the form shows.

import { jsonMutationHeaders } from '$lib/api/csrf';
import { HttpResponseError } from '$lib/api/household';

/** One purchase transaction as the list endpoint returns it (no lines). */
export interface PurchaseSummary {
	/** `purchase_transactions.id`. */
	id: string;
	/** Owning household. */
	household_id: string;
	/** Retailer token, e.g. `tesco`. */
	retailer: string;
	/** Purchase date (`YYYY-MM-DD`). */
	purchase_date: string;
	/** `manual` for a purchase typed by hand, `import` for an import. */
	source_kind: string;
	/** The retailer's own id, or the generated UUID for a manual purchase. */
	source_transaction_id: string | null;
	/** `source_id` or `fingerprint`. */
	identity_kind: string;
	/** The computed content fingerprint (always present). */
	fingerprint: string;
	/** ISO-4217 currency code. */
	currency: string;
	/** Total in integer cents. */
	total_cents: number;
	/** Current revision; 1 until a correction. */
	revision: number;
	/** First write time (RFC 3339 UTC). */
	created_at: string;
	/** Last write time (RFC 3339 UTC). */
	updated_at: string;
	/** Number of lines on the current revision. */
	line_count: number;
	/** Linked `receipt_images.id`, or `null` when no photo is linked. */
	receipt_image_id: string | null;
}

/** One line of a purchase. */
export interface PurchaseLine {
	/** `purchase_lines.id`. */
	id: string;
	/** Owning transaction. */
	transaction_id: string;
	/** Position within the transaction (0-based). */
	line_ordinal: number;
	/** The line text exactly as printed. */
	original_text: string;
	/** Product name, when known. */
	name: string | null;
	/** Brand, when known. */
	brand: string | null;
	/** Quantity bought (decimal for weight/volume). */
	quantity: number | null;
	/** Pack size value. */
	size_value: number | null;
	/** Pack size unit. */
	size_unit: string | null;
	/** Unit price in integer cents. */
	unit_price_cents: number | null;
	/** Line total in integer cents. */
	line_total_cents: number;
	/** Discount on the line in integer cents (may be negative for a manual line). */
	line_discount_cents: number;
	/** Deposit in integer cents. */
	deposit_cents: number;
	/** Optional retailer-product link. */
	retailer_product_id: string | null;
	/** Optional canonical-item link. */
	canonical_item_id: string | null;
	/** `unmatched` or `matched`. */
	match_status: string;
	/** Revision this line belongs to. */
	revision: number;
	/** Write time (RFC 3339 UTC). */
	created_at: string;
}

/** One superseded revision of a purchase. */
export interface PurchaseRevision {
	/** The revision number that was superseded. */
	revision: number;
	/** Retailer at that revision. */
	retailer: string;
	/** Purchase date at that revision. */
	purchase_date: string;
	/** Currency at that revision. */
	currency: string;
	/** Total in integer cents at that revision. */
	total_cents: number;
	/** Source payload at that revision. */
	source_payload: string;
	/** When the revision was superseded (RFC 3339 UTC). */
	superseded_at: string;
	/** The lines at that revision, as stored JSON. */
	lines: unknown;
}

/** One purchase with its current lines and its superseded revisions. */
export interface PurchaseDetail extends PurchaseSummary {
	/** Original payload stored for the current revision. */
	source_payload: string;
	lines: PurchaseLine[];
	revisions: PurchaseRevision[];
}

/** One line as the form submits it; money already in integer cents. */
export interface PurchaseLineInput {
	/** The line text as printed. */
	original_text: string;
	/** Quantity bought, or `null` when left blank. */
	quantity: number | null;
	/** Line total in integer cents (non-negative). */
	line_total_cents: number;
	/** Discount in integer cents; may be negative for a manual surcharge. */
	line_discount_cents: number;
}

/** A manual purchase as the form submits it. */
export interface PurchaseInput {
	/** Retailer, free text; the server trims and lowercases it. */
	retailer: string;
	/** Purchase date (`YYYY-MM-DD`). */
	purchase_date: string;
	/** The printed receipt total in cents, or `null` to use the line sum. */
	total_cents: number | null;
	/** Linked receipt photo, or `null` for no link. */
	receipt_image_id: string | null;
	/** The purchase's lines. */
	lines: PurchaseLineInput[];
}

/** The `{ "errors": [...] }` body a 422 from these routes carries. */
export interface PurchaseValidationErrors {
	errors: string[];
}

/**
 * A 422 from the manual-purchase routes, carrying the server's per-field
 * messages (e.g. `lines[0].line_total_cents: expected an integer`) so the form
 * can point at the field rather than show one generic failure. Extends
 * [`HttpResponseError`] so a caller's existing status handling still applies.
 */
export class PurchaseValidationError extends HttpResponseError {
	constructor(
		/** The server's per-field messages. */
		public readonly messages: string[]
	) {
		super(422, 'purchase validation failed (422)');
		this.name = 'PurchaseValidationError';
	}
}

function base(householdId: string): string {
	return `/api/household/${encodeURIComponent(householdId)}/purchases`;
}

/**
 * List the household's purchases from `GET /api/household/{id}/purchases` (no
 * lines, oldest purchase date first). Throws on a non-OK response.
 */
export async function listPurchases(householdId: string): Promise<PurchaseSummary[]> {
	const res = await fetch(base(householdId));
	if (!res.ok) {
		throw new HttpResponseError(res.status, `purchases failed (${res.status})`);
	}
	return (await res.json()) as PurchaseSummary[];
}

/**
 * Fetch one purchase with its lines and revisions from
 * `GET /api/household/{id}/purchases/{txid}`. Throws on a non-OK response.
 */
export async function getPurchase(
	householdId: string,
	transactionId: string
): Promise<PurchaseDetail> {
	const res = await fetch(`${base(householdId)}/${encodeURIComponent(transactionId)}`);
	if (!res.ok) {
		throw new HttpResponseError(res.status, `purchase failed (${res.status})`);
	}
	return (await res.json()) as PurchaseDetail;
}

/**
 * Add a hand-entered purchase via `POST /api/household/{id}/purchases/manual`
 * (CSRF-protected). Returns the stored purchase, lines and revisions.
 */
export async function createManualPurchase(
	householdId: string,
	input: PurchaseInput
): Promise<PurchaseDetail> {
	const res = await fetch(`${base(householdId)}/manual`, {
		method: 'POST',
		headers: jsonMutationHeaders(),
		body: JSON.stringify(input)
	});
	if (!res.ok) {
		throw await purchaseError(res, 'create purchase failed');
	}
	return (await res.json()) as PurchaseDetail;
}

/**
 * Correct a hand-entered purchase via `PUT /api/household/{id}/purchases/{txid}`
 * (CSRF-protected). The server keeps the purchase's identity, so this corrects
 * the same row and yields a new revision rather than a second purchase.
 */
export async function updateManualPurchase(
	householdId: string,
	transactionId: string,
	input: PurchaseInput
): Promise<PurchaseDetail> {
	const res = await fetch(`${base(householdId)}/${encodeURIComponent(transactionId)}`, {
		method: 'PUT',
		headers: jsonMutationHeaders(),
		body: JSON.stringify(input)
	});
	if (!res.ok) {
		throw await purchaseError(res, 'update purchase failed');
	}
	return (await res.json()) as PurchaseDetail;
}

/**
 * Set or clear the purchase's linked receipt photo via
 * `PUT /api/household/{id}/purchases/{txid}/receipt` (CSRF-protected). `null`
 * clears the link. Returns the updated purchase.
 */
export async function setPurchaseReceipt(
	householdId: string,
	transactionId: string,
	receiptImageId: string | null
): Promise<PurchaseDetail> {
	const res = await fetch(`${base(householdId)}/${encodeURIComponent(transactionId)}/receipt`, {
		method: 'PUT',
		headers: jsonMutationHeaders(),
		body: JSON.stringify({ receipt_image_id: receiptImageId })
	});
	if (!res.ok) {
		throw await purchaseError(res, 'link receipt failed');
	}
	return (await res.json()) as PurchaseDetail;
}

/**
 * Delete one purchase via `DELETE /api/household/{id}/purchases/{txid}`
 * (CSRF-protected). Its lines and revision history go with it.
 */
export async function deletePurchase(householdId: string, transactionId: string): Promise<void> {
	const res = await fetch(`${base(householdId)}/${encodeURIComponent(transactionId)}`, {
		method: 'DELETE',
		headers: jsonMutationHeaders()
	});
	if (!res.ok) {
		throw new HttpResponseError(res.status, `delete purchase failed (${res.status})`);
	}
}

/**
 * Build the error for a failed manual call. A 422 carries the server's field
 * messages (`{ "errors": [...] }`) so the form can show which field is wrong;
 * anything else is a plain status error.
 */
async function purchaseError(res: Response, fallback: string): Promise<HttpResponseError> {
	if (res.status === 422) {
		let messages: string[] = [];
		try {
			const body = (await res.json()) as PurchaseValidationErrors;
			if (Array.isArray(body.errors)) messages = body.errors;
		} catch {
			messages = [];
		}
		return new PurchaseValidationError(messages);
	}
	return new HttpResponseError(res.status, `${fallback} (${res.status})`);
}
