<script lang="ts">
	// Bulk-add modal (issue #115): paste a multi-line grocery message, get one
	// editable proposed entry per line, then confirm to queue them through the
	// normal add path.
	//
	// The modal is a PREVIEW only: nothing is enqueued until Confirm, and the
	// clipboard is never read — parsing only ever sees the textarea's value, which
	// the user pasted or typed. It reuses `parseGroceryText` + `evaluateProposals`,
	// so counts/sizes and duplicates follow exactly the rules the single-item
	// input already uses.

	import {
		parseGroceryText,
		evaluateProposals,
		type DuplicateSource
	} from '$lib/bulk/parse';
	import type { ActiveItem } from '$lib/matching/duplicate';
	import { MAX_QUANTITY } from '$lib/stores/list';

	interface Props {
		/** Active list items, for duplicate detection against the live list. */
		activeItems: ActiveItem[];
		/** Queue the accepted entries; resolves once the batch is enqueued. */
		onconfirm: (items: Array<{ rawText: string; quantity: number }>) => void | Promise<void>;
		/** Dismiss without adding anything. */
		oncancel: () => void;
	}

	let { activeItems, onconfirm, oncancel }: Props = $props();

	/** An editable preview row: parsed entry plus its per-row duplicate decision. */
	interface Row {
		id: string;
		text: string;
		quantity: number;
		/** Duplicate rows are excluded unless the user explicitly opts in. */
		addAnyway: boolean;
	}

	let pasted = $state('');
	let rows = $state<Row[]>([]);
	let submitting = $state(false);
	// Set once the batch has been handed to the caller. Blocks a re-submit even if
	// the caller (unusually) leaves the modal mounted, so the same rows can never
	// be enqueued twice.
	let submitted = $state(false);
	let error = $state('');

	// Rebuild the preview whenever the pasted text changes; row edits elsewhere
	// (text/quantity/addAnyway) live in `rows` and persist until the paste changes.
	$effect(() => {
		const source = pasted;
		rows = parseGroceryText(source).map((entry) => ({ ...entry, addAnyway: false }));
	});

	const proposals = $derived(evaluateProposals(rows, activeItems));
	const proposalById = $derived(new Map(proposals.map((proposal) => [proposal.id, proposal])));

	/**
	 * A row is added unless it is a duplicate the user has not opted into. This
	 * mirrors the single-item rule that a duplicate is never added without an
	 * explicit "Add anyway"; flagged rows stay visible with that option.
	 */
	function isIncluded(row: Row): boolean {
		const proposal = proposalById.get(row.id);
		return proposal?.duplicate.found ? row.addAnyway : true;
	}

	/** A row is confirmable only with non-empty text and a positive whole count. */
	function isValid(row: Row): boolean {
		return row.text.trim() !== '' && Number.isFinite(row.quantity) && row.quantity >= 1;
	}

	const includedRows = $derived(rows.filter(isIncluded));

	const canConfirm = $derived(
		!submitting && !submitted && includedRows.length > 0 && includedRows.every(isValid)
	);

	/** Human-readable duplicate explanation, or '' when the row is not a duplicate. */
	function duplicateLabel(row: Row): string {
		const proposal = proposalById.get(row.id);
		if (!proposal || !proposal.duplicate.found) return '';
		const name = proposal.duplicate.matchedItem.display_name;
		const label: Record<DuplicateSource, string> = {
			list: `Already on your list: “${name}”`,
			batch: `Duplicate in this paste: “${name}”`
		};
		return proposal.duplicateSource ? label[proposal.duplicateSource] : '';
	}

	function removeRow(id: string) {
		rows = rows.filter((row) => row.id !== id);
	}

	async function confirm() {
		if (!canConfirm) return;
		submitting = true;
		error = '';
		try {
			await onconfirm(
				includedRows.map((row) => ({
					rawText: row.text.trim(),
					quantity: Math.min(MAX_QUANTITY, Math.max(1, Math.floor(row.quantity)))
				}))
			);
			submitted = true;
		} catch (err) {
			error = err instanceof Error ? err.message : 'Could not add the pasted items.';
		} finally {
			submitting = false;
		}
	}

	/** Dismissal is refused mid-submit so a queued batch can't be abandoned. */
	function dismiss() {
		if (submitting) return;
		oncancel();
	}

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			dismiss();
		}
	}

	// Only a click on the backdrop itself (not a bubbled click from inside) dismisses.
	function onBackdropClick(event: MouseEvent) {
		if (event.target === event.currentTarget) dismiss();
	}
</script>

<svelte:window onkeydown={onKeydown} />

<!-- Backdrop click + Escape dismiss; the dialog is programmatically focusable
     (tabindex=-1) so the keyboard path is the Escape handler. Mirrors AmbiguityModal. -->
<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
	class="backdrop"
	role="dialog"
	aria-modal="true"
	aria-label="Paste a grocery list"
	tabindex="-1"
	onclick={onBackdropClick}
>
	<div class="modal">
		<h2 class="title">Paste a grocery list</h2>
		<p class="hint" id="bulk-hint">
			One item per line. English or Spanish. “2x milk” sets a quantity; “500g mince”
			keeps its size.
		</p>

		<textarea
			class="paste"
			aria-label="Grocery list text"
			aria-describedby="bulk-hint"
			placeholder={'Milk\n2x eggs\npan'}
			bind:value={pasted}
		></textarea>

		{#if rows.length > 0}
			<p class="summary" aria-live="polite">
				{includedRows.length} of {rows.length} will be added
			</p>
			<ul class="preview" aria-label="Proposed items">
				{#each rows as row, i (row.id)}
					{@const proposal = proposalById.get(row.id)}
					<li class="row" class:excluded={!isIncluded(row)}>
						<div class="fields">
							<input
								class="text"
								type="text"
								aria-label={`Item ${i + 1} text`}
								bind:value={row.text}
							/>
							<input
								class="qty"
								type="number"
								min="1"
								max={MAX_QUANTITY}
								aria-label={`Item ${i + 1} quantity`}
								bind:value={row.quantity}
							/>
							<button
								type="button"
								class="remove"
								aria-label={`Remove item ${i + 1}`}
								onclick={() => removeRow(row.id)}>×</button
							>
						</div>
						{#if proposal?.duplicate.found}
							<p class="duplicate">
								<span>{duplicateLabel(row)}</span>
								<label class="add-anyway">
									<input type="checkbox" bind:checked={row.addAnyway} />
									Add anyway (+{row.quantity})
								</label>
							</p>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}

		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}

		<div class="actions">
			<button type="button" class="confirm" disabled={!canConfirm} onclick={confirm}>
				{submitting
					? 'Adding…'
					: `Add ${includedRows.length} item${includedRows.length === 1 ? '' : 's'}`}
			</button>
			<button type="button" class="cancel" onclick={dismiss} disabled={submitting}>
				Cancel
			</button>
		</div>
	</div>
</div>

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		background: rgba(15, 23, 42, 0.45);
		padding: 1rem;
		z-index: 50;
	}
	.modal {
		width: 100%;
		max-width: 30rem;
		max-height: 90vh;
		overflow-y: auto;
		background: #fff;
		border-radius: 0.75rem;
		padding: 1.25rem;
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
		box-shadow: 0 10px 30px rgba(15, 23, 42, 0.25);
	}
	.title {
		margin: 0;
		font-size: 1.1rem;
	}
	.hint {
		margin: 0;
		font-size: 0.85rem;
		color: var(--color-text-subtle);
	}
	.paste {
		width: 100%;
		min-height: 7rem;
		resize: vertical;
		padding: 0.6rem 0.75rem;
		font: inherit;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius);
	}
	.summary {
		margin: 0;
		font-size: 0.85rem;
		color: var(--color-text-subtle);
	}
	.preview {
		margin: 0;
		padding: 0;
		list-style: none;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.row {
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: 0.5rem;
	}
	.row.excluded {
		background: #fffbeb;
		border-color: #fde68a;
	}
	.fields {
		display: flex;
		gap: 0.5rem;
	}
	.text {
		flex: 1;
		min-width: 0;
		padding: 0.45rem 0.6rem;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius-sm);
	}
	.qty {
		width: 4.5rem;
		padding: 0.45rem 0.6rem;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius-sm);
	}
	.remove {
		border: 1px solid var(--color-border-strong);
		background: var(--color-surface-alt);
		border-radius: var(--radius-sm);
		width: 2rem;
		font-size: 1.1rem;
		line-height: 1;
	}
	.duplicate {
		margin: 0.5rem 0 0;
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
		align-items: center;
		justify-content: space-between;
		font-size: 0.85rem;
		color: #92400e;
	}
	.add-anyway {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		white-space: nowrap;
	}
	.error {
		margin: 0;
		color: var(--color-danger);
		font-size: 0.9rem;
	}
	.actions {
		display: flex;
		gap: 0.5rem;
		justify-content: flex-end;
	}
	.actions button {
		padding: 0.5rem 1rem;
		border-radius: var(--radius);
		border: 1px solid var(--color-border-strong);
		background: var(--color-surface-alt);
		font-size: 0.95rem;
	}
	.confirm {
		background: var(--color-accent);
		border-color: var(--color-accent);
		color: #fff;
		font-weight: 600;
	}
	.confirm:disabled {
		opacity: 0.5;
	}
</style>
