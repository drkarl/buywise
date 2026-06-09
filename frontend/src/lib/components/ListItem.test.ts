import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import ListItem from './ListItem.svelte';
import type { LocalListEntry } from '$lib/db/dexie';

/** A realistic active list entry. */
function mockEntry(overrides: Partial<LocalListEntry> = {}): LocalListEntry {
	return {
		id: 'e1',
		list_id: 'list-1',
		resolution_status: 'unresolved',
		raw_text: 'Milk',
		display_name: 'Milk',
		quantity: 1,
		status: 'active',
		last_updated_revision: 0,
		updated_at: '2026-06-05T00:00:00.000Z',
		...overrides
	};
}

/** The required callback props, defaulted to spies the test can assert on. */
function props(overrides: Record<string, unknown> = {}) {
	return {
		entry: mockEntry(),
		onbought: vi.fn(),
		onskipped: vi.fn(),
		ondelete: vi.fn(),
		...overrides
	};
}

describe('ListItem quantity stepper (issue #60)', () => {
	it('shows the +/- stepper for an active entry when onquantity is wired', () => {
		const onquantity = vi.fn();
		render(ListItem, props({ entry: mockEntry({ quantity: 2 }), onquantity }));

		expect(screen.getByRole('button', { name: /increase quantity/i })).toBeTruthy();
		expect(screen.getByRole('button', { name: /decrease quantity/i })).toBeTruthy();
		expect(screen.getByText('2')).toBeTruthy();
	});

	it('does not show the stepper when onquantity is absent (static ×N instead)', () => {
		render(ListItem, props({ entry: mockEntry({ quantity: 3 }) }));

		expect(screen.queryByRole('button', { name: /increase quantity/i })).toBeNull();
		expect(screen.getByText('×3')).toBeTruthy();
	});

	it('increments to quantity + 1 via the + button', async () => {
		const onquantity = vi.fn();
		render(ListItem, props({ entry: mockEntry({ quantity: 2 }), onquantity }));

		await fireEvent.click(screen.getByRole('button', { name: /increase quantity/i }));

		expect(onquantity).toHaveBeenCalledWith('e1', 3);
	});

	it('decrements to quantity - 1 via the - button', async () => {
		const onquantity = vi.fn();
		render(ListItem, props({ entry: mockEntry({ quantity: 2 }), onquantity }));

		await fireEvent.click(screen.getByRole('button', { name: /decrease quantity/i }));

		expect(onquantity).toHaveBeenCalledWith('e1', 1);
	});

	it('disables the - button at quantity 1 (cannot go below 1)', () => {
		const onquantity = vi.fn();
		render(ListItem, props({ entry: mockEntry({ quantity: 1 }), onquantity }));

		const minus = screen.getByRole('button', { name: /decrease quantity/i });
		expect((minus as HTMLButtonElement).disabled).toBe(true);
	});

	it('hides the stepper for an in-basket (bought) entry even when onquantity is wired', () => {
		const onquantity = vi.fn();
		render(ListItem, props({ entry: mockEntry({ quantity: 2, status: 'bought' }), onquantity }));

		expect(screen.queryByRole('button', { name: /increase quantity/i })).toBeNull();
	});
});
