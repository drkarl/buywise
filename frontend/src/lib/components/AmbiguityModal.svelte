<script lang="ts">
	// Disambiguation modal for an `ambiguous` list entry (P3-T3). Presentational:
	// it renders the candidate items as a radio group and surfaces the user's
	// decision via callback props — all Dexie/mutation logic lives in the parent +
	// store (mirroring ListItem's onbought/onskipped/ondelete shape).
	//
	//   "Confirm"     → onconfirm(chosenCanonicalItemId)
	//   "Keep as-is"  → onkeep()   (dismiss WITHOUT resolving; no server call)
	//
	// "Keep as-is" deliberately does not resolve: the entry stays `ambiguous` so we
	// never spam the server with a non-decision (an acceptance criterion).

	/** One selectable candidate: the canonical item id and a human label. */
	export interface CandidateItem {
		id: string;
		label: string;
	}

	interface Props {
		/** The raw term being disambiguated (e.g. "cream"); shown in the prompt. */
		rawText: string;
		/** Candidate items to choose between. */
		candidates: CandidateItem[];
		/** Confirm the chosen candidate (sends the resolve_alias mutation upstream). */
		onconfirm: (chosenCanonicalItemId: string) => void;
		/** Dismiss without resolving — the entry stays ambiguous. */
		onkeep: () => void;
	}

	let { rawText, candidates, onconfirm, onkeep }: Props = $props();

	// No pre-selection: the user must make an explicit choice before Confirm
	// enables, so we never resolve to an arbitrary default.
	let selectedId = $state<string | null>(null);

	const canConfirm = $derived(selectedId !== null);

	function confirm() {
		if (selectedId === null) return;
		onconfirm(selectedId);
	}

	// Dismissal mirrors "Keep as-is": both Escape and a backdrop (outside-modal)
	// click keep the entry `ambiguous` without resolving — the modal is non-
	// destructive, so the least-surprising dismissal is the same no-op as the
	// explicit "Keep as-is" button (never an implicit resolve).
	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			onkeep();
		}
	}

	// Only a click on the backdrop itself (not a bubbled click from inside the
	// modal) dismisses, so interacting with the candidates/buttons never closes it.
	function onBackdropClick(event: MouseEvent) {
		if (event.target === event.currentTarget) onkeep();
	}
</script>

<svelte:window onkeydown={onKeydown} />

<!-- Backdrop click + Escape (via svelte:window) dismiss to "Keep as-is"; the
     dialog is programmatically focusable (tabindex=-1) so the keyboard path is the
     Escape handler, justifying the static-interaction suppression. -->
<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
	class="backdrop"
	role="dialog"
	aria-modal="true"
	aria-label={`What did you mean by "${rawText}"?`}
	tabindex="-1"
	onclick={onBackdropClick}
>
	<div class="modal">
		<h2 class="prompt">What did you mean by “{rawText}”?</h2>

		<fieldset class="candidates">
			<legend class="sr-only">Candidate items</legend>
			{#each candidates as candidate (candidate.id)}
				<label class="candidate">
					<input
						type="radio"
						name="candidate"
						value={candidate.id}
						checked={selectedId === candidate.id}
						onchange={() => (selectedId = candidate.id)}
					/>
					<span>{candidate.label}</span>
				</label>
			{/each}
		</fieldset>

		<div class="actions">
			<button type="button" class="confirm" disabled={!canConfirm} onclick={confirm}>
				Confirm
			</button>
			<button type="button" class="keep" onclick={() => onkeep()}>Keep as-is</button>
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
		max-width: 22rem;
		background: #fff;
		border-radius: 0.75rem;
		padding: 1.25rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
		box-shadow: 0 10px 30px rgba(15, 23, 42, 0.25);
	}
	.prompt {
		margin: 0;
		font-size: 1.1rem;
	}
	.candidates {
		border: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.candidate {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		padding: 0.5rem 0.6rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		cursor: pointer;
		font-size: 1rem;
	}
	.actions {
		display: flex;
		gap: 0.5rem;
		justify-content: flex-end;
	}
	.actions button {
		padding: 0.5rem 1rem;
		border-radius: 0.5rem;
		border: 1px solid #cbd5e1;
		background: #f8fafc;
		cursor: pointer;
		font-size: 0.95rem;
	}
	.confirm {
		background: #0f766e;
		border-color: #0f766e;
		color: #fff;
	}
	.confirm:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
		border: 0;
	}
</style>
