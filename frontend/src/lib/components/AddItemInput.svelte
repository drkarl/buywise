<script lang="ts">
	// Text input for adding items. The trimmed text is handed to the caller via
	// the `onadd` callback synchronously on submit — deliberately NOT debounced.
	// Debouncing the submit could drop a queued add (a rapid second submit would
	// cancel the first, and an unmount within the window would drop it), which
	// violates the PR's "every action queues immediately" invariant. Clearing the
	// field on submit is what prevents a fast duplicate Enter on the same text.

	interface Props {
		/** Called with the trimmed item text on each add. */
		onadd: (rawText: string) => void | Promise<void>;
		placeholder?: string;
	}

	let { onadd, placeholder = 'Add an item…' }: Props = $props();

	let value = $state('');

	function submit() {
		const text = value.trim();
		if (!text) return;

		// Clear the field before firing so a fast second Enter can't re-submit the
		// same text — the empty value short-circuits the guard above.
		value = '';
		void onadd(text);
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
