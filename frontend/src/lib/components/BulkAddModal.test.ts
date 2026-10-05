import { describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import BulkAddModal from './BulkAddModal.svelte';
import type { ActiveItem } from '$lib/matching/duplicate';

/** Paste `text` into the modal's textarea, driving the same input event a paste does. */
async function paste(text: string): Promise<void> {
	await fireEvent.input(screen.getByLabelText('Grocery list text'), {
		target: { value: text }
	});
}

/** The confirm button's accessible name encodes the number of rows to be added. */
function confirmButton(): HTMLButtonElement {
	return screen.getByRole('button', { name: /^Add \d+ items?$/ }) as HTMLButtonElement;
}

describe('BulkAddModal', () => {
	it('turns each pasted line into an editable proposed row', async () => {
		render(BulkAddModal, { activeItems: [], onconfirm: vi.fn(), oncancel: vi.fn() });

		await paste('Milk\n2x eggs');

		expect((await screen.findByLabelText('Item 1 text') as HTMLInputElement).value).toBe('Milk');
		expect((screen.getByLabelText('Item 2 text') as HTMLInputElement).value).toBe('Eggs');
		// The count is lifted into the editable quantity field.
		expect((screen.getByLabelText('Item 2 quantity') as HTMLInputElement).value).toBe('2');
	});

	it('reads Spanish counts and keeps a size request in the text', async () => {
		render(BulkAddModal, { activeItems: [], onconfirm: vi.fn(), oncancel: vi.fn() });

		await paste('dos huevos\n500g mince');

		expect((await screen.findByLabelText('Item 1 text') as HTMLInputElement).value).toBe('Huevos');
		expect((screen.getByLabelText('Item 1 quantity') as HTMLInputElement).value).toBe('2');
		expect((screen.getByLabelText('Item 2 text') as HTMLInputElement).value).toBe('500g mince');
	});

	it('keeps an unparseable prose line visible rather than silently discarding it', async () => {
		render(BulkAddModal, { activeItems: [], onconfirm: vi.fn(), oncancel: vi.fn() });

		await paste('milk, bread, eggs');

		expect(
			(await screen.findByLabelText('Item 1 text') as HTMLInputElement).value
		).toBe('Milk, bread, eggs');
	});

	it('enqueues nothing until Confirm', async () => {
		const onconfirm = vi.fn();
		render(BulkAddModal, { activeItems: [], onconfirm, oncancel: vi.fn() });

		await paste('Milk\nBread');
		await screen.findByLabelText('Item 1 text');

		expect(onconfirm).not.toHaveBeenCalled();

		await fireEvent.click(screen.getByRole('button', { name: 'Add 2 items' }));

		expect(onconfirm).toHaveBeenCalledTimes(1);
		expect(onconfirm).toHaveBeenCalledWith([
			{ rawText: 'Milk', quantity: 1 },
			{ rawText: 'Bread', quantity: 1 }
		]);
	});

	it('sends edited text and quantity on Confirm', async () => {
		const onconfirm = vi.fn();
		render(BulkAddModal, { activeItems: [], onconfirm, oncancel: vi.fn() });

		await paste('Milk');
		await fireEvent.input(await screen.findByLabelText('Item 1 text'), {
			target: { value: 'Oat milk' }
		});
		await fireEvent.input(screen.getByLabelText('Item 1 quantity'), {
			target: { value: '3' }
		});

		await fireEvent.click(screen.getByRole('button', { name: 'Add 1 item' }));

		expect(onconfirm).toHaveBeenCalledWith([{ rawText: 'Oat milk', quantity: 3 }]);
	});

	it('removes a proposed row before confirming', async () => {
		const onconfirm = vi.fn();
		render(BulkAddModal, { activeItems: [], onconfirm, oncancel: vi.fn() });

		await paste('Milk\nBread');
		await screen.findByLabelText('Item 1 text');

		await fireEvent.click(screen.getByRole('button', { name: 'Remove item 1' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Add 1 item' }));

		expect(onconfirm).toHaveBeenCalledWith([{ rawText: 'Bread', quantity: 1 }]);
	});

	it('flags a duplicate against the active list and excludes it by default', async () => {
		const onconfirm = vi.fn();
		const activeItems: ActiveItem[] = [{ id: 'a1', display_name: 'Milk' }];
		render(BulkAddModal, { activeItems, onconfirm, oncancel: vi.fn() });

		await paste('Milk\nBread');

		expect(await screen.findByText(/Already on your list/)).toBeTruthy();
		// The normal add rule: a duplicate is never added without an explicit
		// choice, so only Bread is queued unless the user opts in.
		await fireEvent.click(screen.getByRole('button', { name: 'Add 1 item' }));
		expect(onconfirm).toHaveBeenCalledWith([{ rawText: 'Bread', quantity: 1 }]);
	});

	it('includes a flagged duplicate once the user ticks "Add anyway"', async () => {
		const onconfirm = vi.fn();
		const activeItems: ActiveItem[] = [{ id: 'a1', display_name: 'Milk' }];
		render(BulkAddModal, { activeItems, onconfirm, oncancel: vi.fn() });

		await paste('Milk\nBread');
		await screen.findByText(/Already on your list/);

		await fireEvent.click(screen.getByRole('checkbox', { name: /Add anyway/ }));
		await fireEvent.click(screen.getByRole('button', { name: 'Add 2 items' }));

		expect(onconfirm).toHaveBeenCalledWith([
			{ rawText: 'Milk', quantity: 1 },
			{ rawText: 'Bread', quantity: 1 }
		]);
	});

	it('flags a duplicate within the pasted batch', async () => {
		render(BulkAddModal, { activeItems: [], onconfirm: vi.fn(), oncancel: vi.fn() });

		await paste('Milk\nMilk');

		expect(await screen.findByText(/Duplicate in this paste/)).toBeTruthy();
		expect(confirmButton().textContent).toMatch(/Add 1 item/);
	});

	it('matches duplicates bilingually against the active list', async () => {
		const activeItems: ActiveItem[] = [{ id: 'a1', display_name: 'Milk' }];
		render(BulkAddModal, { activeItems, onconfirm: vi.fn(), oncancel: vi.fn() });

		await paste('leche');

		expect(await screen.findByText(/Already on your list/)).toBeTruthy();
	});

	it('disables Confirm when there is nothing to add', async () => {
		render(BulkAddModal, { activeItems: [], onconfirm: vi.fn(), oncancel: vi.fn() });

		expect(confirmButton().disabled).toBe(true);

		await paste('- \n   ');
		expect(confirmButton().disabled).toBe(true);
	});

	it('calls oncancel without adding anything', async () => {
		const onconfirm = vi.fn();
		const oncancel = vi.fn();
		render(BulkAddModal, { activeItems: [], onconfirm, oncancel });

		await paste('Milk');
		await screen.findByLabelText('Item 1 text');

		await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));

		expect(oncancel).toHaveBeenCalledTimes(1);
		expect(onconfirm).not.toHaveBeenCalled();
	});

	it('never reads the clipboard — pasted text is the only input', async () => {
		const readText = vi.fn();
		Object.defineProperty(navigator, 'clipboard', {
			value: { readText },
			configurable: true
		});

		render(BulkAddModal, { activeItems: [], onconfirm: vi.fn(), oncancel: vi.fn() });
		await paste('Milk\nBread');
		await screen.findByLabelText('Item 1 text');

		expect(readText).not.toHaveBeenCalled();
	});
});
