import { describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
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
});
