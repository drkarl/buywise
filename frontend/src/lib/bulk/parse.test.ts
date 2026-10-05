import { describe, expect, it } from 'vitest';

import { parseGroceryText, evaluateProposals, type ParsedGroceryLine } from './parse';
import type { ActiveItem } from '$lib/matching/duplicate';

/** Project parsed rows to the fields the assertions care about. */
function rows(pasted: string): Array<{ text: string; quantity: number }> {
	return parseGroceryText(pasted).map(({ text, quantity }) => ({ text, quantity }));
}

/** Build proposed entries directly (bypassing id sequencing) for duplicate tests. */
function entries(...texts: string[]): ParsedGroceryLine[] {
	return texts.map((text, i) => ({ id: `e${i}`, text, quantity: 1 }));
}

describe('parseGroceryText', () => {
	it('turns one line per item into an entry, preserving order', () => {
		expect(rows('Milk\nBread\nEggs')).toEqual([
			{ text: 'Milk', quantity: 1 },
			{ text: 'Bread', quantity: 1 },
			{ text: 'Eggs', quantity: 1 }
		]);
	});

	it('handles CRLF line endings', () => {
		expect(rows('Milk\r\nBread')).toEqual([
			{ text: 'Milk', quantity: 1 },
			{ text: 'Bread', quantity: 1 }
		]);
	});

	it('drops blank and whitespace-only lines', () => {
		expect(rows('  \nMilk\n\n   \nBread\n')).toEqual([
			{ text: 'Milk', quantity: 1 },
			{ text: 'Bread', quantity: 1 }
		]);
	});

	it('strips bullets, checkboxes and ordinals', () => {
		expect(rows('- Milk\n• 2x eggs\n1. Bread\n[ ] Butter\n2) Cheese')).toEqual([
			{ text: 'Milk', quantity: 1 },
			{ text: 'Eggs', quantity: 2 },
			{ text: 'Bread', quantity: 1 },
			{ text: 'Butter', quantity: 1 },
			{ text: 'Cheese', quantity: 1 }
		]);
	});

	it('drops a line that is only a list marker', () => {
		expect(rows('- \nMilk')).toEqual([{ text: 'Milk', quantity: 1 }]);
	});

	it('does not treat a decimal size as an ordinal marker', () => {
		expect(rows('2.5kg potatoes')).toEqual([{ text: '2.5kg potatoes', quantity: 1 }]);
	});

	describe('quantities', () => {
		it('lifts an English count prefix into quantity', () => {
			expect(rows('2x milk')).toEqual([{ text: 'Milk', quantity: 2 }]);
		});

		it('lifts a trailing count', () => {
			expect(rows('milk x2')).toEqual([{ text: 'Milk', quantity: 2 }]);
		});

		it('lifts a bare leading number', () => {
			expect(rows('3 apples')).toEqual([{ text: 'Apples', quantity: 3 }]);
		});

		it('lifts a word-number', () => {
			expect(rows('three apples')).toEqual([{ text: 'Apples', quantity: 3 }]);
		});

		it('is case-insensitive for word-numbers', () => {
			expect(rows('TWO eggs')).toEqual([{ text: 'Eggs', quantity: 2 }]);
		});

		it('lifts Spanish counts', () => {
			expect(rows('dos huevos\ntres manzanas')).toEqual([
				{ text: 'Huevos', quantity: 2 },
				{ text: 'Manzanas', quantity: 3 }
			]);
		});

		it('lifts the apocopated Spanish "una"', () => {
			expect(rows('una botella')).toEqual([{ text: 'Botella', quantity: 1 }]);
		});

		it('keeps a lone count token as the item text', () => {
			expect(rows('2')).toEqual([{ text: '2', quantity: 1 }]);
		});
	});

	describe('sizes stay in the text (the protocol has no size field)', () => {
		it('keeps a joined size', () => {
			expect(rows('500g mince')).toEqual([{ text: '500g mince', quantity: 1 }]);
		});

		it('keeps a spaced size', () => {
			expect(rows('500 g rice')).toEqual([{ text: '500 g rice', quantity: 1 }]);
		});

		it('keeps a Spanish size', () => {
			expect(rows('dos litros leche')).toEqual([{ text: 'Dos litros leche', quantity: 1 }]);
		});

		it('keeps an imperial unit and makes it text', () => {
			expect(rows('1lb butter')).toEqual([{ text: '1lb butter', quantity: 1 }]);
		});

		it('lifts a count while keeping a size in the same line', () => {
			expect(rows('2x 500g milk')).toEqual([{ text: '500g milk', quantity: 2 }]);
		});
	});

	describe('ambiguous / unparseable input is surfaced, never discarded', () => {
		it('keeps a prose line as a single editable entry', () => {
			expect(rows('whatever the neighbours asked for')).toEqual([
				{ text: 'Whatever the neighbours asked for', quantity: 1 }
			]);
		});

		it('keeps comma-separated prose as ONE entry (no prose splitting)', () => {
			expect(rows('milk, bread, eggs')).toEqual([
				{ text: 'Milk, bread, eggs', quantity: 1 }
			]);
		});

		it('keeps an emoji-only-ish line', () => {
			expect(rows('milk 🥛')).toEqual([{ text: 'Milk 🥛', quantity: 1 }]);
		});

		it('preserves accents in display text', () => {
			expect(rows('Jamón')).toEqual([{ text: 'Jamón', quantity: 1 }]);
		});
	});
});

describe('evaluateProposals — duplicates', () => {
	it('flags no entry when everything is distinct', () => {
		const result = evaluateProposals(entries('Milk', 'Bread'), []);
		expect(result.map((r) => r.duplicate.found)).toEqual([false, false]);
		expect(result.map((r) => r.duplicateSource)).toEqual([null, null]);
	});

	it('flags a duplicate against the active list with the matched item', () => {
		const active: ActiveItem[] = [{ id: 'a1', display_name: 'Milk' }];
		const [result] = evaluateProposals(entries('Milk'), active);
		expect(result.duplicate).toMatchObject({
			found: true,
			matchType: 'exact',
			matchedItem: { id: 'a1', display_name: 'Milk' }
		});
		expect(result.duplicateSource).toBe('list');
	});

	it('matches the active list bilingually', () => {
		const active: ActiveItem[] = [{ id: 'a1', display_name: 'Milk' }];
		const [result] = evaluateProposals(entries('Leche'), active);
		expect(result.duplicate).toMatchObject({ found: true, matchType: 'dictionary' });
		expect(result.duplicateSource).toBe('list');
	});

	it('flags a duplicate within the pasted batch (first occurrence wins)', () => {
		const result = evaluateProposals(entries('Milk', 'Milk'), []);
		expect(result[0].duplicate.found).toBe(false);
		expect(result[1].duplicate).toMatchObject({
			found: true,
			matchedItem: { display_name: 'Milk' }
		});
		expect(result[1].duplicateSource).toBe('batch');
	});

	it('flags every later repeat, not just the second', () => {
		const result = evaluateProposals(entries('Milk', 'Milk', 'Milk'), []);
		expect(result.map((r) => r.duplicate.found)).toEqual([false, true, true]);
		expect(result.map((r) => r.duplicateSource)).toEqual([null, 'batch', 'batch']);
	});

	it('matches batch duplicates bilingually', () => {
		const result = evaluateProposals(entries('Leche', 'Milk'), []);
		expect(result[1].duplicate).toMatchObject({ found: true, matchType: 'dictionary' });
		expect(result[1].duplicateSource).toBe('batch');
	});

	it('prefers a list match over a batch match', () => {
		const active: ActiveItem[] = [{ id: 'a1', display_name: 'Milk' }];
		const result = evaluateProposals(entries('Milk', 'Milk'), active);
		expect(result.map((r) => r.duplicateSource)).toEqual(['list', 'list']);
	});

	it('treats different size requests as distinct entries', () => {
		const result = evaluateProposals(entries('1kg rice', '500g rice'), []);
		expect(result.map((r) => r.duplicate.found)).toEqual([false, false]);
	});

	it('treats a size request and an unsized entry as distinct', () => {
		const result = evaluateProposals(entries('rice', '500g rice'), []);
		expect(result.map((r) => r.duplicate.found)).toEqual([false, false]);
	});

	it('re-evaluates from the surviving rows when an earlier one is removed', () => {
		// Model a user removing the first "Milk": the remaining pair has the first
		// occurrence unflagged and the second flagged.
		const result = evaluateProposals(entries('Milk', 'Milk'), []);
		expect(result.map((r) => r.duplicateSource)).toEqual([null, 'batch']);
	});
});
