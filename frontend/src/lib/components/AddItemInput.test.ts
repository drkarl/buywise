import { describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import AddItemInput from './AddItemInput.svelte';

/** Type `text` into the input and submit the form. */
async function add(input: HTMLElement, text: string) {
	await fireEvent.input(input, { target: { value: text } });
	await fireEvent.submit(input.closest('form')!);
}

describe('AddItemInput', () => {
	it('fires onadd synchronously with the trimmed text', async () => {
		const onadd = vi.fn();
		render(AddItemInput, { onadd });

		await add(screen.getByLabelText('Add an item'), '  Milk  ');

		expect(onadd).toHaveBeenCalledTimes(1);
		expect(onadd).toHaveBeenCalledWith('Milk');
	});

	it('does not drop a rapid second add — both submissions queue (no debounce)', async () => {
		const onadd = vi.fn();
		render(AddItemInput, { onadd });
		const input = screen.getByLabelText('Add an item');

		// Two submits back-to-back within what used to be the debounce window.
		await add(input, 'Eggs');
		await add(input, 'Bread');

		expect(onadd).toHaveBeenCalledTimes(2);
		expect(onadd).toHaveBeenNthCalledWith(1, 'Eggs');
		expect(onadd).toHaveBeenNthCalledWith(2, 'Bread');
	});

	it('clears the field on submit so a duplicate Enter on the same text is a no-op', async () => {
		const onadd = vi.fn();
		render(AddItemInput, { onadd });
		const input = screen.getByLabelText('Add an item') as HTMLInputElement;

		await add(input, 'Cheese');
		expect(input.value).toBe('');

		// A second submit without retyping (empty field) must not re-fire.
		await fireEvent.submit(input.closest('form')!);
		expect(onadd).toHaveBeenCalledTimes(1);
	});

	it('ignores whitespace-only submissions', async () => {
		const onadd = vi.fn();
		render(AddItemInput, { onadd });

		await add(screen.getByLabelText('Add an item'), '   ');

		expect(onadd).not.toHaveBeenCalled();
	});

	describe('duplicate warning (Phase 2, client-side)', () => {
		const milkList = [{ id: '1', display_name: 'Milk' }];

		it('adding "leche" when "Milk" is active shows the warning and holds the add', async () => {
			const onadd = vi.fn();
			render(AddItemInput, { onadd, activeItems: milkList });

			await add(screen.getByLabelText('Add an item'), 'leche');

			// Warning renders, referencing the existing item, and the add is held.
			const alert = await screen.findByRole('alert');
			expect(alert.textContent).toMatch(/Milk/);
			expect(alert.textContent).toMatch(/already on your list/i);
			expect(onadd).not.toHaveBeenCalled();
		});

		it('"Add anyway" commits the held text via the add path', async () => {
			const onadd = vi.fn();
			render(AddItemInput, { onadd, activeItems: milkList });

			await add(screen.getByLabelText('Add an item'), 'leche');
			await screen.findByRole('alert');

			await fireEvent.click(screen.getByRole('button', { name: /Add anyway/i }));

			expect(onadd).toHaveBeenCalledTimes(1);
			expect(onadd).toHaveBeenCalledWith('leche');
			// Warning dismissed after committing.
			expect(screen.queryByRole('alert')).toBeNull();
		});

		it('"Merge quantity" is disabled and carries an accessible explanatory note', async () => {
			const onadd = vi.fn();
			render(AddItemInput, { onadd, activeItems: milkList });

			await add(screen.getByLabelText('Add an item'), 'leche');
			await screen.findByRole('alert');

			const merge = screen.getByRole('button', { name: /Merge quantity/i }) as HTMLButtonElement;
			expect(merge.disabled).toBe(true);
			// Associated explanatory text (not just the disabled state) referencing #60.
			const describedBy = merge.getAttribute('aria-describedby');
			expect(describedBy).toBeTruthy();
			const note = document.getElementById(describedBy!);
			expect(note?.textContent).toMatch(/#60/);

			// It must not trigger the add path.
			await fireEvent.click(merge);
			expect(onadd).not.toHaveBeenCalled();
		});

		it('"Cancel" dismisses the warning without adding', async () => {
			const onadd = vi.fn();
			render(AddItemInput, { onadd, activeItems: milkList });

			await add(screen.getByLabelText('Add an item'), 'leche');
			await screen.findByRole('alert');

			await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));

			expect(onadd).not.toHaveBeenCalled();
			await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
		});

		it('does not warn for a genuinely distinct item ("bread")', async () => {
			const onadd = vi.fn();
			render(AddItemInput, { onadd, activeItems: milkList });

			await add(screen.getByLabelText('Add an item'), 'bread');

			// No warning, and the add fires synchronously through the normal path.
			expect(screen.queryByRole('alert')).toBeNull();
			expect(onadd).toHaveBeenCalledTimes(1);
			expect(onadd).toHaveBeenCalledWith('bread');
		});
	});
});
