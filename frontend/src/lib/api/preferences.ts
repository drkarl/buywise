// Client types + helpers for per-item household product preferences (issue #102).
//
// These mirror the wire shape of `GET`/`PUT /api/items/{id}/preferences` exactly —
// the serde serialisation of `crates/api/src/preferences.rs::PreferencesResponse`
// and the enums in `crates/core/src/domain.rs`. Field names are the serialized
// (snake_case) names; do not rename them here or the JSON won't bind. Retailer ids
// are the lowercase DB strings shared with `$lib/stores/shopping`.

import { jsonMutationHeaders } from '$lib/api/csrf';
import type { RetailerId } from '$lib/stores/shopping';

/** How strictly an item constrains matching products. */
export type ConstraintPolicy =
	| 'any'
	| 'brand'
	| 'allowed_set'
	| 'own_label'
	| 'exact'
	| 'ask';

/** Whether substitutions are allowed, must be asked about, or are forbidden. */
export type SubstitutionPolicy = 'any' | 'ask' | 'strict';

/** SI basis for a normalised size. */
export type SizeBasis = 'g' | 'ml' | 'each';

/** One product match for the item, joined to its global retailer product. */
export interface MatchView {
	retailer_product_id: string;
	status: 'accepted' | 'rejected' | 'suggested' | 'pending';
	matched_by: string;
	name: string;
	brand: string | null;
	retailer: RetailerId;
	size_text: string | null;
}

/** The item's effective constraint fields (the optimiser-enforced subset). */
export interface PreferencesConstraints {
	brand: string | null;
	allowed_brands: string[];
	excluded_brands: string[];
	preferred_size_value: number | null;
	preferred_size_basis: SizeBasis | null;
	substitution_policy: SubstitutionPolicy;
}

/** Why a candidate was ranked the way it was (stable signal codes). */
export type MatchSignal = 'name' | 'brand' | 'size';

/** Ingest confidence of a candidate's offer (mirrors `SourceConfidence`). */
export type SourceConfidence = 'high' | 'medium' | 'low';

/**
 * One proposed retailer product the household can match to the item (issue #108).
 * Carries the product identity and the price provenance the decision is based on.
 * A candidate is only proposed when it has a current offer and is not already
 * accepted/suggested/rejected for the item.
 */
export interface CandidateView {
	retailer_product_id: string;
	retailer: RetailerId;
	name: string;
	brand: string | null;
	size_text: string | null;
	pack_count: number | null;
	score: number;
	signals: MatchSignal[];
	price_cents: number;
	loyalty_price_cents: number | null;
	loyalty_scheme: string | null;
	unit_price_eur_micros: number | null;
	unit_price_basis: string | null;
	offer_snapshot_date: string | null;
	fetched_at: string;
	source_confidence: SourceConfidence;
}

/** The full `GET`/`PUT /api/items/{id}/preferences` response body. */
export interface Preferences {
	canonical_item_id: string;
	name_en: string;
	name_es: string;
	default_constraint_policy: ConstraintPolicy;
	constraints: PreferencesConstraints;
	matches: MatchView[];
	/** Proposed products per retailer, best match first (issue #108). */
	candidates: CandidateView[];
	/**
	 * Stable warning codes for data the optimiser cannot evaluate:
	 * `brand_policy_missing_brand`, `allowed_set_empty`, `no_usable_matches`.
	 */
	warnings: string[];
}

/** Body for `PUT /api/items/{id}/preferences`. Replaces the whole preference set. */
export interface PreferencesUpdate {
	default_constraint_policy: ConstraintPolicy;
	brand: string | null;
	allowed_brands: string[];
	excluded_brands: string[];
	preferred_size_value: number | null;
	preferred_size_basis: SizeBasis | null;
	substitution_policy: SubstitutionPolicy;
}

/**
 * Fetch an item's preferences + product matches from
 * `GET /api/items/{id}/preferences`. A GET needs no CSRF token. Throws on a
 * non-OK response so the caller can show an error state (the error body is not a
 * stable contract, so only the status is surfaced).
 */
export async function fetchPreferences(canonicalItemId: string): Promise<Preferences> {
	const res = await fetch(`/api/items/${encodeURIComponent(canonicalItemId)}/preferences`);
	if (!res.ok) {
		throw new Error(`preferences failed (${res.status})`);
	}
	return (await res.json()) as Preferences;
}

/**
 * Replace an item's preferences via `PUT /api/items/{id}/preferences`
 * (CSRF-protected). Returns the effective preferences after the save. Throws on a
 * non-OK response — a 422 means the settings are inconsistent (e.g. a brand policy
 * with no brand), which the caller should prevent before sending.
 */
export async function savePreferences(
	canonicalItemId: string,
	body: PreferencesUpdate
): Promise<Preferences> {
	const res = await fetch(`/api/items/${encodeURIComponent(canonicalItemId)}/preferences`, {
		method: 'PUT',
		headers: jsonMutationHeaders(),
		body: JSON.stringify(body)
	});
	if (!res.ok) {
		throw new Error(`preferences update failed (${res.status})`);
	}
	return (await res.json()) as Preferences;
}
