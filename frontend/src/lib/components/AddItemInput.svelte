<script lang="ts">
	// Text input for adding items. The trimmed text is handed to the caller via
	// the `onadd` callback synchronously on submit — deliberately NOT debounced.
	// Debouncing the submit could drop a queued add (a rapid second submit would
	// cancel the first, and an unmount within the window would drop it), which
	// violates the PR's "every action queues immediately" invariant. Clearing the
	// field on submit is what prevents a fast duplicate Enter on the same text.
	//
	// Phase 2 adds client-side duplicate detection (dictionary-only, no server
	// call): on submit we first run `checkForDuplicate` against the active items.
	// When a match is found we DON'T add immediately — we surface an inline
	// `DuplicateWarning` and let the user choose "Add anyway" or "Cancel". When no
	// match is found the original synchronous, field-clearing add path is used
	// unchanged, so the "every action queues immediately" invariant is preserved
	// for the common (non-duplicate) case.

	import { checkForDuplicate, type DuplicateMatch, type ActiveItem } from '$lib/matching/duplicate';
	import DuplicateWarning from '$lib/components/DuplicateWarning.svelte';

	interface Props {
		/** Called with the trimmed item text on each add. */
		onadd: (rawText: string) => void | Promise<void>;
		/** Active list items to check new input against (Phase 2: match on display_name). */
		activeItems?: ActiveItem[];
		placeholder?: string;
	}

	let { onadd, activeItems = [], placeholder = 'Add an item…' }: Props = $props();

	let value = $state('');

	// Pending duplicate prompt. When set, the inline warning is shown and the add
	// is held until the user resolves it. `pendingText` is the trimmed text that
	// triggered the prompt (the input has already been cleared, matching the
	// no-debounce field-clear guard).
	let pendingMatch = $state<DuplicateMatch | null>(null);
	let pendingText = $state('');

	/** Commit the add and clear any pending duplicate prompt. */
	function commit(text: string) {
		pendingMatch = null;
		pendingText = '';
		void onadd(text);
	}

	function submit() {
		const text = value.trim();
		if (!text) return;

		// Clear the field before doing anything else so a fast second Enter can't
		// re-submit the same text — the empty value short-circuits the guard above.
		value = '';

		// Client-side, dictionary-only duplicate check (no network).
		const result = checkForDuplicate(text, activeItems);
		if (result.duplicate.found) {
			// Hold the add and surface the inline warning; the user decides.
			pendingMatch = result.duplicate;
			pendingText = text;
			return;
		}

		// No duplicate: original synchronous add path, unchanged.
		commit(text);
	}

	function onsubmit(event: SubmitEvent) {
		event.preventDefault();
		submit();
	}
</script>

<form class="add-item" {onsubmit}>
	<input
		type="text"
		bind:value
		{placeholder}
		aria-label="Add an item"
		autocomplete="off"
		enterkeyhint="done"
	/>
	<button type="submit" disabled={!value.trim()} aria-label="Add item">Add</button>
</form>

{#if pendingMatch}
	<DuplicateWarning
		newItemText={pendingText}
		match={pendingMatch}
		onaddanyway={() => commit(pendingText)}
		oncancel={() => {
			pendingMatch = null;
			pendingText = '';
		}}
	/>
{/if}

<style>
	.add-item {
		display: flex;
		gap: 0.5rem;
	}
	input {
		flex: 1;
		padding: 0.6rem 0.75rem;
		font-size: 1rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.5rem;
	}
	button {
		padding: 0.6rem 1rem;
		font-size: 1rem;
		font-weight: 600;
		color: #fff;
		background: #0f766e;
		border: none;
		border-radius: 0.5rem;
		cursor: pointer;
	}
	button:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
