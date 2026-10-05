<script lang="ts">
	// Store selector for shopping mode: a row of the five retailers. Selecting one
	// switches the category ordering (the page re-groups via `sortByStoreOrder`).
	// Presentational + a single `onselect` callback; the page owns the selection
	// state and persistence.

	import { RETAILERS, type RetailerId } from '$lib/stores/shopping';

	interface Props {
		selected: RetailerId;
		/**
		 * The stores to offer. Defaults to all five retailers; the trip-aware
		 * shopping mode (issue #104) passes just the chosen plan's stores so the
		 * shopper can only switch between the stores they are actually visiting.
		 */
		stores?: readonly { id: RetailerId; label: string }[];
		onselect: (store: RetailerId) => void;
	}

	let { selected, stores = RETAILERS, onselect }: Props = $props();
</script>

<div class="stores" role="group" aria-label="Choose a store">
	{#each stores as retailer (retailer.id)}
		<button
			type="button"
			class="store"
			class:active={retailer.id === selected}
			aria-pressed={retailer.id === selected}
			onclick={() => onselect(retailer.id)}
		>
			{retailer.label}
		</button>
	{/each}
</div>

<style>
	.stores {
		display: flex;
		gap: 0.5rem;
		overflow-x: auto;
		padding-bottom: 0.25rem;
	}
	.store {
		flex: 0 0 auto;
		padding: 0.4rem 0.85rem;
		border: 1px solid #cbd5e1;
		border-radius: 999px;
		background: #f8fafc;
		font-size: 0.9rem;
		cursor: pointer;
		white-space: nowrap;
	}
	.store.active {
		background: #0f766e;
		border-color: #0f766e;
		color: #fff;
	}
</style>
