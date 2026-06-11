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

/**
 * jsdom doesn't implement Element.setPointerCapture, so it can't be spied on
 * directly. Define it as a mock fn on the element and return the mock so tests
 * can assert whether/when the component grabbed pointer capture.
 */
function stubPointerCapture(el: Element) {
	const capture = vi.fn();
	Object.defineProperty(el, 'setPointerCapture', {
		value: capture,
		configurable: true,
		writable: true
	});
	return capture;
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

describe('ListItem tap vs swipe gesture (issue #84)', () => {
	// A real tap is pointerdown → pointerup → click. The swipe surface used to grab
	// pointer capture on pointerdown, which retargeted the click off the inner
	// button so onbought/onskipped never fired. These cases assert a tap on a child
	// button still reaches that button's handler.

	it('fires onbought when the Check off button receives a real pointer tap', async () => {
		const onbought = vi.fn();
		render(ListItem, props({ onbought }));

		const check = screen.getByRole('button', { name: /check off/i });
		await fireEvent.pointerDown(check, { pointerId: 1, clientX: 5 });
		await fireEvent.pointerUp(check, { pointerId: 1, clientX: 5 });
		await fireEvent.click(check);

		expect(onbought).toHaveBeenCalledWith('e1');
	});

	it('fires onskipped when the Skip button receives a real pointer tap', async () => {
		const onskipped = vi.fn();
		render(ListItem, props({ onskipped }));

		const skip = screen.getByRole('button', { name: /^skip/i });
		await fireEvent.pointerDown(skip, { pointerId: 1, clientX: 5 });
		await fireEvent.pointerUp(skip, { pointerId: 1, clientX: 5 });
		await fireEvent.click(skip);

		expect(onskipped).toHaveBeenCalledWith('e1');
	});

	it('does not arm swipe-to-delete for a pointerdown that lands on a child button', async () => {
		const ondelete = vi.fn();
		render(ListItem, props({ ondelete }));

		// A pointerdown on the button followed by a large left drag must NOT trigger
		// the delete gesture: the surface skips the gesture for taps on a child.
		const check = screen.getByRole('button', { name: /check off/i });
		const swipe = screen.getByRole('group', { name: 'Milk' });
		await fireEvent.pointerDown(check, { pointerId: 1, clientX: 200 });
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 0 });
		await fireEvent.pointerUp(swipe, { pointerId: 1, clientX: 0 });

		expect(ondelete).not.toHaveBeenCalled();
	});

	it('does not capture the pointer when the gesture starts on a child button', async () => {
		// jsdom doesn't model pointer-capture click retargeting, so the click-based
		// tests above can't catch a regression to unconditional capture. Spy on the
		// surface's setPointerCapture and assert it is never called for a
		// button-start pointerdown, even across a large drag.
		render(ListItem, props());

		const check = screen.getByRole('button', { name: /check off/i });
		const swipe = screen.getByRole('group', { name: 'Milk' });
		const capture = stubPointerCapture(swipe);

		await fireEvent.pointerDown(check, { pointerId: 1, clientX: 200 });
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 0 });
		await fireEvent.pointerUp(swipe, { pointerId: 1, clientX: 0 });

		expect(capture).not.toHaveBeenCalled();
	});

	it('still triggers swipe-to-delete when the drag starts on the row surface', async () => {
		const ondelete = vi.fn();
		render(ListItem, props({ ondelete }));

		// Pointerdown on the bare name (not a button) then a left drag past the
		// threshold must still delete — the gesture is preserved for real swipes.
		const name = screen.getByText('Milk');
		const swipe = screen.getByRole('group', { name: 'Milk' });
		await fireEvent.pointerDown(name, { pointerId: 1, clientX: 200 });
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 0 });
		await fireEvent.pointerUp(swipe, { pointerId: 1, clientX: 0 });

		expect(ondelete).toHaveBeenCalledWith('e1');
	});

	it('defers setPointerCapture until a drag exceeds DRAG_SLOP', async () => {
		// Capture must NOT be grabbed on pointerdown (that is what retargeted the tap
		// in issue #84). It is acquired only once a real drag (> DRAG_SLOP, 6px) is
		// detected on pointermove — and exactly once.
		render(ListItem, props());

		const name = screen.getByText('Milk');
		const swipe = screen.getByRole('group', { name: 'Milk' });
		const capture = stubPointerCapture(swipe);

		await fireEvent.pointerDown(name, { pointerId: 1, clientX: 100 });
		expect(capture).not.toHaveBeenCalled();

		// A 4px move stays within the slop: still no capture.
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 96 });
		expect(capture).not.toHaveBeenCalled();

		// Past the slop: capture grabbed once with the active pointerId.
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 90 });
		expect(capture).toHaveBeenCalledTimes(1);
		expect(capture).toHaveBeenCalledWith(1);

		// Further moves don't re-capture.
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 80 });
		expect(capture).toHaveBeenCalledTimes(1);
	});

	it('a sub-DRAG_SLOP move neither deletes nor captures the pointer', async () => {
		// Pinning DRAG_SLOP: a move smaller than 6px must not start a drag — no
		// delete is fired and the pointer is never captured.
		const ondelete = vi.fn();
		render(ListItem, props({ ondelete }));

		const name = screen.getByText('Milk');
		const swipe = screen.getByRole('group', { name: 'Milk' });
		const capture = stubPointerCapture(swipe);

		await fireEvent.pointerDown(name, { pointerId: 1, clientX: 100 });
		// 5px left — within the slop.
		await fireEvent.pointerMove(swipe, { pointerId: 1, clientX: 95 });
		await fireEvent.pointerUp(swipe, { pointerId: 1, clientX: 95 });

		expect(capture).not.toHaveBeenCalled();
		expect(ondelete).not.toHaveBeenCalled();
	});
});

describe('ListItem remove + move-back controls', () => {
	it('calls ondelete when the visible × Remove button is clicked (active row)', async () => {
		const ondelete = vi.fn();
		render(ListItem, props({ ondelete }));

		await fireEvent.click(screen.getByRole('button', { name: 'Remove Milk' }));
		expect(ondelete).toHaveBeenCalledWith('e1');
	});

	it('does not show a move-back button on an active row', () => {
		render(ListItem, props({ onmoveback: vi.fn() }));
		expect(screen.queryByRole('button', { name: /move milk back to the list/i })).toBeNull();
	});

	it('shows a move-back button on a bought (basket) row and calls onmoveback', async () => {
		const onmoveback = vi.fn();
		render(
			ListItem,
			props({ entry: mockEntry({ status: 'bought' }), onmoveback })
		);

		await fireEvent.click(
			screen.getByRole('button', { name: 'Move Milk back to the list' })
		);
		expect(onmoveback).toHaveBeenCalledWith('e1');
	});

	it('keeps the × Remove button available on a basket row', async () => {
		const ondelete = vi.fn();
		render(
			ListItem,
			props({ entry: mockEntry({ status: 'skipped' }), ondelete, onmoveback: vi.fn() })
		);

		await fireEvent.click(screen.getByRole('button', { name: 'Remove Milk' }));
		expect(ondelete).toHaveBeenCalledWith('e1');
	});
});
