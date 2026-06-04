<script lang="ts">
	// Shopping mode (P2-T6). Renders entirely off Dexie live queries so it works
	// fully offline: store selection, category grouping, and the progress bar all
	// update without any network. Checking an item off reuses the same
	// `markBought` mutation as the main list view (optimistic + queued).
	//
	// SSR/prerender are disabled globally in `src/routes/+layout.ts`, so this
	// dynamic `[id]` route needs no per-route override.

	import { page } from '$app/state';
	import StoreSelector from '$lib/components/StoreSelector.svelte';
	import CategoryGroup from '$lib/components/CategoryGroup.svelte';
	import ProgressBar from '$lib/components/ProgressBar.svelte';
	import { markBought } from '$lib/stores/list';
	import {
		shoppingEntries,
		categories,
		storeCategoryOrders,
		loadSelectedStore,
		saveSelectedStore,
		type RetailerId
	} from '$lib/stores/shopping';
	import { sortByStoreOrder } from '$lib/shopping/sortByStoreOrder';
	import { startSync, type SyncHandle } from '$lib/sync';

	const listId = $derived(page.params.id ?? '');

	// Re-derive the live stores whenever the route id changes. Categories and
	// store orders are list-independent (household-wide), but deriving them here
	// keeps every subscription scoped to this page's lifecycle.
	const entries = $derived(shoppingEntries(listId));
	const cats = $derived(categories());
	const orders = $derived(storeCategoryOrders());

	let selectedStore = $state<RetailerId>(loadSelectedStore());

	function selectStore(store: RetailerId) {
		selectedStore = store;
		saveSelectedStore(store);
	}

	// Group + order items for the selected store. Categories with zero ACTIVE
	// items are HIDDEN (product decision, AC #5): once everything in a category is
	// bought, the section disappears so the shopper focuses on what's left.
	// Bought items in a still-active category remain visible struck-through.
	const groups = $derived(
		sortByStoreOrder($entries, $orders, selectedStore, $cats).filter(
			(g) => g.remaining > 0
		)
	);

	const total = $derived($entries.length);
	const bought = $derived($entries.filter((e) => e.status === 'bought').length);

	// Start the sync engine so check-offs flush when online and remote changes
	// stream in; identical lifecycle to the main list view.
	let sync: SyncHandle | null = null;
	$effect(() => {
		if (!listId) return;
		const handle = startSync(listId);
		sync = handle;
		return () => {
			handle.stop();
			if (sync === handle) sync = null;
		};
	});
</script>

<svelte:head>
	<title>BuyWise — Shopping</title>
</svelte:head>

<main class="shopping-view">
	<header class="shopping-header">
		<h1>Shopping</h1>
		<a class="back" href="/list/{listId}">Back to list</a>
	</header>

	<StoreSelector selected={selectedStore} onselect={selectStore} />

	<ProgressBar {bought} {total} />

	{#if groups.length > 0}
		<div class="groups">
			{#each groups as group (group.category_id)}
				<CategoryGroup {group} onbought={(id) => markBought(listId, id)} />
			{/each}
		</div>
	{:else if total === 0}
		<p class="empty">Nothing to shop for — add items to your list first.</p>
	{:else}
		<p class="empty">All done! Every item is checked off.</p>
	{/if}
</main>

<style>
	.shopping-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.shopping-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.shopping-header h1 {
		font-size: 1.4rem;
		margin: 0;
	}
	.back {
		font-size: 0.9rem;
		color: #0f766e;
	}
	.groups {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.empty {
		padding: 1.5rem;
		text-align: center;
		color: #94a3b8;
	}
</style>
