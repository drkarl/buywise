<script lang="ts">
	// Text input for adding items. Submission is debounced so rapid Enter presses
	// (or programmatic submits) collapse into a single add, and the caller is
	// handed the trimmed text via the `onadd` callback prop.

	interface Props {
		/** Called with the trimmed item text when a (debounced) add fires. */
		onadd: (rawText: string) => void | Promise<void>;
		/** Debounce window in ms; 0 disables debouncing (handy for tests). */
		debounceMs?: number;
		placeholder?: string;
	}

	let { onadd, debounceMs = 250, placeholder = 'Add an item…' }: Props = $props();

	let value = $state('');
	let timer: ReturnType<typeof setTimeout> | undefined;

	function submit() {
		const text = value.trim();
		if (!text) return;

		const fire = () => {
			void onadd(text);
			value = '';
		};

		if (debounceMs <= 0) {
			fire();
			return;
		}

		clearTimeout(timer);
		timer = setTimeout(fire, debounceMs);
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
