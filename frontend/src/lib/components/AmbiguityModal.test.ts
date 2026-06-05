import { describe, expect, it, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import AmbiguityModal from './AmbiguityModal.svelte';

const candidates = [
	{ id: 'ci-single', label: 'Single cream' },
	{ id: 'ci-double', label: 'Double cream' },
	{ id: 'ci-sour', label: 'Sour cream' }
];

function renderModal(overrides: { onconfirm?: () => void; onkeep?: () => void } = {}) {
	const onconfirm = overrides.onconfirm ?? vi.fn();
	const onkeep = overrides.onkeep ?? vi.fn();
	render(AmbiguityModal, { rawText: 'cream', candidates, onconfirm, onkeep });
	return { onconfirm, onkeep };
}

describe('AmbiguityModal', () => {
	it('lists exactly the candidate items', () => {
		renderModal();

		for (const c of candidates) {
			expect(screen.getByText(c.label)).toBeTruthy();
		}
		// One radio per candidate, no more.
		expect(screen.getAllByRole('radio')).toHaveLength(candidates.length);
	});

	it('shows the raw term in the prompt', () => {
		renderModal();
		expect(screen.getByRole('dialog').getAttribute('aria-label')).toMatch(/cream/);
	});

	it('Confirm is disabled until a candidate is selected', async () => {
		renderModal();
		const confirm = screen.getByRole('button', { name: 'Confirm' }) as HTMLButtonElement;
		expect(confirm.disabled).toBe(true);

		await fireEvent.click(screen.getByRole('radio', { name: 'Double cream' }));
		expect(confirm.disabled).toBe(false);
	});

	it('Confirm emits onconfirm with the chosen canonical item id', async () => {
		const { onconfirm, onkeep } = renderModal();

		await fireEvent.click(screen.getByRole('radio', { name: 'Double cream' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Confirm' }));

		expect(onconfirm).toHaveBeenCalledTimes(1);
		expect(onconfirm).toHaveBeenCalledWith('ci-double');
		// Confirm is the resolve path; it must NOT also fire the keep callback.
		expect(onkeep).not.toHaveBeenCalled();
	});

	it('"Keep as-is" emits onkeep and never resolves (no onconfirm)', async () => {
		const { onconfirm, onkeep } = renderModal();

		await fireEvent.click(screen.getByRole('button', { name: 'Keep as-is' }));

		expect(onkeep).toHaveBeenCalledTimes(1);
		expect(onconfirm).not.toHaveBeenCalled();
	});

	it('does not emit onconfirm when Confirm is clicked with no selection', async () => {
		const { onconfirm } = renderModal();

		// Clicking the disabled button must not resolve to an arbitrary default.
		await fireEvent.click(screen.getByRole('button', { name: 'Confirm' }));
		expect(onconfirm).not.toHaveBeenCalled();
	});

	it('Escape dismisses via onkeep without resolving', async () => {
		const { onconfirm, onkeep } = renderModal();

		await fireEvent.keyDown(window, { key: 'Escape' });

		expect(onkeep).toHaveBeenCalledTimes(1);
		expect(onconfirm).not.toHaveBeenCalled();
	});

	it('a backdrop click (outside the modal) dismisses via onkeep without resolving', async () => {
		const { onconfirm, onkeep } = renderModal();

		// Click the backdrop element itself (the dialog), not its inner content.
		await fireEvent.click(screen.getByRole('dialog'));

		expect(onkeep).toHaveBeenCalledTimes(1);
		expect(onconfirm).not.toHaveBeenCalled();
	});

	it('a click inside the modal content does NOT dismiss', async () => {
		const { onkeep } = renderModal();

		// Clicking a candidate label bubbles to the backdrop, but the handler only
		// dismisses when the click target IS the backdrop — so this must not close.
		await fireEvent.click(screen.getByText('Double cream'));

		expect(onkeep).not.toHaveBeenCalled();
	});
});
