// Per-store category rule helpers (issue #99).
//
// The settings screen edits a store's allowed categories as checkboxes. Rules
// are matched most-specific-first, so a sub-category rule overrides its category
// rule and anything with no matching rule is allowed. To keep what we send (and
// store) minimal, we normalise a selection back to only the rules that actually
// change the outcome: an "allowed" rule at the default scope, or a sub-category
// rule that agrees with its category, carries no information and is dropped.

import type { CategoryRule } from '$lib/api/settings';

/** Whether an item in `categoryId` / optional `subcategoryId` is allowed. */
export function isAllowed(
	rules: readonly CategoryRule[],
	categoryId: string,
	subcategoryId: string | null = null
): boolean {
	if (subcategoryId !== null) {
		const sub = rules.find(
			(r) => r.category_id === categoryId && r.subcategory_id === subcategoryId
		);
		if (sub) return sub.allowed;
	}
	const cat = rules.find((r) => r.category_id === categoryId && r.subcategory_id === null);
	return cat ? cat.allowed : true;
}

/**
 * Return a new rule list with the given scope set to `allowed`, normalised so
 * redundant rules are dropped and the order is deterministic (category id, then
 * whole-category before its sub-categories).
 */
export function withScope(
	rules: readonly CategoryRule[],
	categoryId: string,
	subcategoryId: string | null,
	allowed: boolean
): CategoryRule[] {
	const merged = rules
		.filter((r) => !(r.category_id === categoryId && r.subcategory_id === subcategoryId))
		.concat({ category_id: categoryId, subcategory_id: subcategoryId, allowed });

	// Effective whole-category value for each category, before normalisation.
	const categoryAllowed = (categoryId: string): boolean => {
		const parent = merged.find(
			(r) => r.category_id === categoryId && r.subcategory_id === null
		);
		return parent ? parent.allowed : true;
	};

	const kept: CategoryRule[] = [];
	for (const rule of merged) {
		if (rule.subcategory_id === null) {
			// A whole-category "allowed" rule is the default and carries nothing.
			if (!rule.allowed) kept.push(rule);
		} else {
			// Keep a sub-category rule only when it refines its category.
			if (rule.allowed !== categoryAllowed(rule.category_id)) kept.push(rule);
		}
	}

	return kept.sort((a, b) => {
		if (a.category_id !== b.category_id) return a.category_id < b.category_id ? -1 : 1;
		if (a.subcategory_id === b.subcategory_id) return 0;
		if (a.subcategory_id === null) return -1;
		if (b.subcategory_id === null) return 1;
		return a.subcategory_id < b.subcategory_id ? -1 : 1;
	});
}
