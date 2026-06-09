// Client types + helpers for the basket optimiser (P4-T3).
//
// These mirror the wire shape of `POST /api/optimise` exactly — the serde
// serialisation of `crates/api/src/optimise.rs::OptimiseResponse` and the plan
// structs in `crates/core/src/optimiser.rs`. Field names are the serialized
// names (snake_case for the response/enum variants); do not rename them here or
// the JSON won't bind. Retailer ids are the lowercase DB strings that also back
// `RETAILERS` in `$lib/stores/shopping`.

import { RETAILERS, type RetailerId } from '$lib/stores/shopping';
import { jsonMutationHeaders } from '$lib/api/csrf';

/** Freshness confidence for a matched offer line (lowercase, as serialized). */
export type FreshnessConfidence = 'high' | 'low' | 'unknown';

/** One priced line in a store plan (one resolved list item at a retailer). */
export interface PlanLine {
	item_index: number;
	display_text: string;
	/**
	 * The resolved canonical item id this line prices. Always present for a priced
	 * line (unresolved items never appear in a plan); paired with
	 * `retailer_product_id` to accept/reject the underlying product match via
	 * `POST /api/match-feedback` (P5-T1).
	 */
	canonical_item_id: string | null;
	retailer: RetailerId;
	retailer_product_id: string;
	offer_id: string;
	unit_price_cents: number;
	line_total_cents: number;
	loyalty_applied: boolean;
	freshness_confidence: FreshnessConfidence;
}

/**
 * A concrete shopping plan: which store(s) to visit and the priced lines.
 * `total_cents` is integer cents and EXCLUDES any missing items (those are
 * counted by `missing_count`). When `dunnes_voucher_offer_text` is non-null the
 * plan benefits from a Dunnes voucher, surfaced inline in the UI.
 */
export interface StorePlan {
	stores: RetailerId[];
	lines: PlanLine[];
	total_cents: number;
	missing_count: number;
	dunnes_voucher_offer_text: string | null;
}

/** The optimiser's routed recommendation (single- vs two-store split). */
export interface Recommendation {
	kind: 'single_store' | 'two_store';
	plan: StorePlan;
	clears_convenience_threshold: boolean;
	two_store_saving_cents: number;
}

/** A list item the optimiser could not resolve to a catalogue item. */
export interface UnresolvedItem {
	item_index: number;
	display_text: string;
}

/** Why a resolved item still has no usable offer in any plan. */
export type NoValidOfferReason =
	| 'no_matches'
	| 'constraints_eliminated_all'
	| 'brand_policy_missing_brand'
	// Backlog-T1: substitution_policy=strict and the exact matched product has no
	// qualifying offer, so the item is shown as missing rather than substituted.
	| 'strict_no_exact_offer';

/** A resolved item that produced no valid offer, with the reason. */
export interface NoValidOfferItem {
	item_index: number;
	display_text: string;
	reason: NoValidOfferReason;
}

/**
 * Backlog-T1: a resolved, priceable item held back for household confirmation
 * because its `substitution_policy = 'ask'`. Excluded from every plan until the
 * household chooses a product.
 */
export interface NeedsConfirmationItem {
	item_index: number;
	display_text: string;
	candidate_count: number;
}

/** The full `POST /api/optimise` 200 response body. */
export interface OptimiseResponse {
	run_id: string;
	best_single: StorePlan | null;
	best_two_store: StorePlan | null;
	theoretical_minimum: StorePlan | null;
	recommended: Recommendation | null;
	unresolved_items: UnresolvedItem[];
	no_valid_offer_items: NoValidOfferItem[];
	// Backlog-T1: `substitution_policy = 'ask'` items, held out of every plan
	// until the household confirms a product. Surfaced in the optimise page's
	// "Waiting for your confirmation" section; the `strict_no_exact_offer` reason
	// renders in the "excluded from totals" list via `reasonText`. The actual
	// product-choice flow (candidate offers in the payload + a selection action)
	// is tracked in #78 — this payload only carries a count for now.
	needs_confirmation_items: NeedsConfirmationItem[];
	plain_language_explanation: string;
	offer_snapshot_date: string | null;
}

/** Lowercase retailer id → display label (reuses the shopping store's table). */
const RETAILER_LABELS: Record<RetailerId, string> = Object.fromEntries(
	RETAILERS.map((r) => [r.id, r.label])
) as Record<RetailerId, string>;

/**
 * Human label for a retailer id (e.g. "supervalu" → "SuperValu"). Falls back to
 * the raw id for any unknown value so the UI never renders blank.
 */
export function retailerLabel(id: string): string {
	return RETAILER_LABELS[id as RetailerId] ?? id;
}

/**
 * Format integer cents as a Euro string, e.g. 1234 → "€12.34". Negative values
 * are rendered with a leading minus inside the symbol (e.g. -50 → "-€0.50") so a
 * "saving" of zero-or-less reads naturally. Kept tiny and local — the repo has
 * no shared currency helper yet.
 */
export function formatEuros(cents: number): string {
	const sign = cents < 0 ? '-' : '';
	const euros = (Math.abs(cents) / 100).toFixed(2);
	return `${sign}€${euros}`;
}

/** A user's feedback on a product match, mirroring the API's `action` field. */
export type MatchFeedbackAction = 'accept' | 'reject' | 'prefer';

/**
 * POST a user's accept/reject/prefer decision on a product match to
 * `POST /api/match-feedback` (P5-T1). Stamps the `X-CSRF-Token` header (the
 * backend 403s mutating calls without it) and resolves to the response's OK flag
 * so callers can surface a failure without coupling to the (non-stable) error
 * body. Throws only on a network/transport failure, which callers catch.
 */
export async function sendMatchFeedback(args: {
	canonicalItemId: string;
	retailerProductId: string;
	action: MatchFeedbackAction;
}): Promise<{ ok: boolean }> {
	const res = await fetch('/api/match-feedback', {
		method: 'POST',
		headers: jsonMutationHeaders(),
		body: JSON.stringify({
			canonical_item_id: args.canonicalItemId,
			retailer_product_id: args.retailerProductId,
			action: args.action
		})
	});
	return { ok: res.ok };
}
