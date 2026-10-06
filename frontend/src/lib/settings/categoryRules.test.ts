import { describe, expect, it } from 'vitest';
import { isAllowed, withScope } from './categoryRules';
import type { CategoryRule } from '$lib/api/settings';

const catOff = (category_id: string): CategoryRule => ({
	category_id,
	subcategory_id: null,
	allowed: false
});
const subOff = (category_id: string, subcategory_id: string): CategoryRule => ({
	category_id,
	subcategory_id,
	allowed: false
});
const subOn = (category_id: string, subcategory_id: string): CategoryRule => ({
	category_id,
	subcategory_id,
	allowed: true
});

describe('isAllowed', () => {
	it('allows everything by default', () => {
		expect(isAllowed([], 'produce')).toBe(true);
		expect(isAllowed([], 'pantry', 'breakfast')).toBe(true);
	});

	it('applies a whole-category exclusion', () => {
		expect(isAllowed([catOff('produce')], 'produce')).toBe(false);
		expect(isAllowed([catOff('produce')], 'pantry')).toBe(true);
	});

	it('lets a sub-category rule override its category', () => {
		const rules = [catOff('pantry'), subOn('pantry', 'breakfast')];
		expect(isAllowed(rules, 'pantry', 'breakfast')).toBe(true);
		expect(isAllowed(rules, 'pantry', 'pasta')).toBe(false);
	});

	it('excludes a sub-category within an allowed category', () => {
		expect(isAllowed([subOff('pantry', 'breakfast')], 'pantry', 'breakfast')).toBe(false);
		expect(isAllowed([subOff('pantry', 'breakfast')], 'pantry', 'pasta')).toBe(true);
	});
});

describe('withScope', () => {
	it('stores a category exclusion and drops it again when re-allowed', () => {
		const off = withScope([], 'produce', null, false);
		expect(off).toEqual([catOff('produce')]);
		expect(withScope(off, 'produce', null, true)).toEqual([]);
	});

	it('keeps only the sub-category rule needed to refine its category', () => {
		// Exclude Pantry, then re-allow Breakfast: both rules are needed.
		const blocked = withScope([], 'pantry', null, false);
		const refined = withScope(blocked, 'pantry', 'breakfast', true);
		expect(refined).toEqual([catOff('pantry'), subOn('pantry', 'breakfast')]);

		// Exclude the sub-category only when the category is allowed.
		expect(withScope([], 'pantry', 'breakfast', false)).toEqual([
			subOff('pantry', 'breakfast')
		]);

		// A sub-category exclusion that merely agrees with its excluded category
		// is redundant.
		const stillBlocked = withScope(blocked, 'pantry', 'breakfast', false);
		expect(stillBlocked).toEqual([catOff('pantry')]);
	});

	it('drops a sub-category exclusion that agrees with its excluded category', () => {
		// Excluding the whole category makes the sub-category exclusion redundant.
		const rules = withScope([subOff('pantry', 'breakfast')], 'pantry', null, false);
		expect(rules).toEqual([catOff('pantry')]);
	});

	it('orders rules deterministically by category', () => {
		let rules = withScope([], 'produce', null, false);
		rules = withScope(rules, 'pantry', null, false);
		rules = withScope(rules, 'pantry', 'breakfast', true);
		expect(rules).toEqual([
			catOff('pantry'),
			subOn('pantry', 'breakfast'),
			catOff('produce')
		]);
	});
});
