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
	import SyncIndicator from '$lib/components/SyncIndicator.svelte';
	import { markBought, pendingEntityIds } from '$lib/stores/list';
	import {
		shoppingEntries,
		categories,
		subcategories,
		storeCategoryOrders,
		loadSelectedStore,
		saveSelectedStore,
		type RetailerId
	} from '$lib/stores/shopping';
	import { sortByStoreOrder } from '$lib/shopping/sortByStoreOrder';
	import { startSync, deriveSyncStatus, type SyncHandle, type SyncStatus } from '$lib/sync';

	const listId = $derived(page.params.id ?? '');

	// Re-derive the live stores whenever the route id changes. Categories and
	// store orders are list-independent (household-wide), but deriving them here
	// keeps every subscription scoped to this page's lifecycle.
	const entries = $derived(shoppingEntries(listId));
	const pendingIds = $derived(pendingEntityIds(listId));
	const cats = $derived(categories());
	const subs = $derived(subcategories());
	const orders = $derived(storeCategoryOrders());

	let selectedStore = $state<RetailerId>(loadSelectedStore());

	function selectStore(store: RetailerId) {
		selectedStore = store;
		saveSelectedStore(store);
	}

	// Group + order items for the selected store. Broad categories split into
	// their sub-categories (issue #101); sections with zero ACTIVE items are
	// HIDDEN (product decision, AC #5): once everything in a section is bought,
	// it disappears so the shopper focuses on what's left. Bought items in a
	// still-active section remain visible struck-through.
	const groups = $derived(
		sortByStoreOrder($entries, $orders, selectedStore, $cats, $subs).filter(
			(g) => g.remaining > 0
		)
	);

	const total = $derived($entries.length);
	const bought = $derived($entries.filter((e) => e.status === 'bought').length);

	// Start the sync engine so check-offs flush when online and remote changes
	// stream in; identical lifecycle to the main list view. The engine's live
	// status drives the always-visible indicator in the header.
	let sync: SyncHandle | null = null;
	let syncStatus = $state<SyncStatus>(deriveSyncStatus());
	$effect(() => {
		if (!listId) return;
		const handle = startSync(listId);
		const unsubscribeStatus = handle.status.subscribe((next) => (syncStatus = next));
		sync = handle;
		return () => {
			unsubscribeStatus();
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
		<div class="shopping-header-meta">
			<SyncIndicator status={syncStatus} ondismiss={() => void sync?.dismissFailures()} />
			<a class="back" href="/list/{listId}">Back to list</a>
		</div>
	</header>

	<StoreSelector selected={selectedStore} onselect={selectStore} />

	<ProgressBar {bought} {total} />

	{#if groups.length > 0}
		<div class="groups">
			{#each groups as group (group.id)}
				<CategoryGroup
					{group}
					pendingIds={$pendingIds}
					onbought={(id) => markBought(listId, id)}
				/>
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
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.shopping-header h1 {
		font-size: 1.4rem;
		margin: 0;
	}
	.shopping-header-meta {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		flex-wrap: wrap;
		justify-content: flex-end;
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
