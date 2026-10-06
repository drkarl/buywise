import { describe, expect, it } from 'vitest';
import { parseEurosToCents, toEurosInput } from './money';

describe('parseEurosToCents', () => {
	it('accepts a dot or a comma as the decimal separator', () => {
		expect(parseEurosToCents('12.34')).toBe(1234);
		expect(parseEurosToCents('12,34')).toBe(1234);
		expect(parseEurosToCents('€12.34')).toBe(1234);
		expect(parseEurosToCents('  12,5 ')).toBe(1250);
	});

	it('handles whole numbers, leading and trailing separators and blanks', () => {
		expect(parseEurosToCents('12')).toBe(1200);
		expect(parseEurosToCents('12.')).toBe(1200);
		expect(parseEurosToCents('.5')).toBe(50);
		expect(parseEurosToCents('0')).toBe(0);
		expect(parseEurosToCents('')).toBeNull();
		expect(parseEurosToCents('   ')).toBeNull();
	});

	it('rounds on the third decimal digit instead of truncating', () => {
		expect(parseEurosToCents('1.005')).toBe(101);
		expect(parseEurosToCents('1.004')).toBe(100);
		expect(parseEurosToCents('1.999')).toBe(200);
		expect(parseEurosToCents('1,239')).toBe(124);
	});

	it('returns the sign so the caller can allow or refuse it', () => {
		expect(parseEurosToCents('-1,50')).toBe(-150);
		expect(parseEurosToCents('-0.01')).toBe(-1);
	});

	it('refuses text, thousands-grouped amounts and double separators', () => {
		expect(parseEurosToCents('abc')).toBeNull();
		expect(parseEurosToCents('1,234,56')).toBeNull();
		expect(parseEurosToCents('1.2.3')).toBeNull();
		expect(parseEurosToCents('12,3.4')).toBeNull();
		expect(parseEurosToCents('1e3')).toBeNull();
	});
});

describe('toEurosInput', () => {
	it('renders cents as a plain two-decimal euro string', () => {
		expect(toEurosInput(1234)).toBe('12.34');
		expect(toEurosInput(0)).toBe('0.00');
		expect(toEurosInput(5)).toBe('0.05');
		expect(toEurosInput(-150)).toBe('-1.50');
	});

	it('round-trips through the parser', () => {
		for (const cents of [0, 5, 99, 100, 1234, -150]) {
			expect(parseEurosToCents(toEurosInput(cents))).toBe(cents);
		}
	});
});
