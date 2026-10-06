// Client types + helpers for household settings (issue #98).
//
// These mirror the wire shape of `GET`/`PATCH /api/settings` exactly — the serde
// serialisation of `crates/api/src/settings.rs::SettingsResponse` and the
// `StoreStatus` enum in `crates/core/src/domain.rs`. Field names are the
// serialized (snake_case) names; do not rename them here or the JSON won't bind.
// Retailer ids are the lowercase DB strings shared with `$lib/stores/shopping`.

import { jsonMutationHeaders } from '$lib/api/csrf';
import type { RetailerId } from '$lib/stores/shopping';

/** How a household wants a store treated during plan generation. */
export type StoreStatus = 'active' | 'occasional' | 'off';

/**
 * One per-store category rule (issue #99): whether the store may fulfil items in
 * `category_id`, or — when `subcategory_id` is set — only that sub-category of
 * it. Rules match most-specific-first; an item with no matching rule is allowed.
 */
export interface CategoryRule {
	/** The category this rule targets (a `categories.id`). */
	category_id: string;
	/** When set, the rule applies only to this sub-category of `category_id`. */
	subcategory_id: string | null;
	/** Whether the store may fulfil items in this scope. */
	allowed: boolean;
}

/** One retailer's household setting. */
export interface StoreSetting {
	/** The retailer this setting applies to (lowercase DB id). */
	retailer: RetailerId;
	/**
	 * `active` is always planned for; `occasional` only when the household marks
	 * a trip to it for a run; `off` is never planned for.
	 */
	status: StoreStatus;
	/** True when the store is close by, lowering the second-store threshold. */
	nearby: boolean;
	/** Per-store category rules (issue #99). Empty ⇒ every category is allowed. */
	category_rules: CategoryRule[];
}

/** A category in the shared catalogue (mirrors `domain.rs::Category`). */
export interface Category {
	id: string;
	name_en: string;
	name_es: string;
	icon: string;
	default_order: number;
}

/** A sub-category in the shared catalogue (mirrors `domain.rs::Subcategory`). */
export interface Subcategory {
	id: string;
	category_id: string;
	name_en: string;
	name_es: string;
	default_order: number;
}

/** The full `GET /api/settings` 200 response body (also returned by `PATCH`). */
export interface Settings {
	/** Second-store convenience threshold in cents. */
	convenience_threshold_2_cents: number;
	/** Third-store convenience threshold in cents. */
	convenience_threshold_3_cents: number;
	/** One entry per retailer, always all five. */
	stores: StoreSetting[];
	/** Shared category catalogue, for rendering the per-store rules. */
	categories: Category[];
	/** Shared sub-category catalogue, for sub-category refinements. */
	subcategories: Subcategory[];
}

/** A partial per-store update; an absent field keeps its stored value. */
export interface StoreSettingPatch {
	retailer: RetailerId;
	status?: StoreStatus;
	nearby?: boolean;
	/**
	 * Replacement category rules: when present the store's whole rule set is
	 * replaced (an empty array clears every rule). Omit to leave it unchanged.
	 */
	category_rules?: CategoryRule[];
}

/** Body for a settings update; at least one field must be present. */
export interface SettingsPatch {
	convenience_threshold_2_cents?: number;
	convenience_threshold_3_cents?: number;
	stores?: StoreSettingPatch[];
}

/**
 * Fetch the household's settings from `GET /api/settings`. The session cookie is
 * sent automatically (same-origin); a GET needs no CSRF token. Throws on a
 * non-OK response so the caller can show an error state — the error body is not
 * a stable contract, so only the status is surfaced.
 */
export async function fetchSettings(): Promise<Settings> {
	const res = await fetch('/api/settings');
	if (!res.ok) {
		throw new Error(`settings failed (${res.status})`);
	}
	return (await res.json()) as Settings;
}

/**
 * Update the household's settings via `PATCH /api/settings` (CSRF-protected).
 * Returns the effective settings after the merge. Throws on a non-OK response.
 */
export async function updateSettings(patch: SettingsPatch): Promise<Settings> {
	const res = await fetch('/api/settings', {
		method: 'PATCH',
		headers: jsonMutationHeaders(),
		body: JSON.stringify(patch)
	});
	if (!res.ok) {
		throw new Error(`settings update failed (${res.status})`);
	}
	return (await res.json()) as Settings;
}
