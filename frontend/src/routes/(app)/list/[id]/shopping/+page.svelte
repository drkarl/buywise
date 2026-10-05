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
		RETAILERS,
		shoppingEntries,
		categories,
		subcategories,
		storeCategoryOrders,
		loadSelectedStore,
		saveSelectedStore,
		type RetailerId
	} from '$lib/stores/shopping';
	import { sortByStoreOrder } from '$lib/shopping/sortByStoreOrder';
	import { loadChosenTrip } from '$lib/shopping/chosenTrip';
	import { retailerLabel } from '$lib/api/optimise';
	import { startSync, deriveSyncStatus, type SyncHandle, type SyncStatus } from '$lib/sync';

	const listId = $derived(page.params.id ?? '');

	// The household's chosen plan (issue #104), if any. When present, shopping mode
	// is scoped to the plan: only its stores are offered, and each shows only that
	// store's allocated items — still a VIEW over the shared entries, so a
	// check-off here updates the same list through the normal offline queue.
	const trip = $derived(loadChosenTrip(listId));
	const storeOptions = $derived(
		trip
			? trip.allocation.stores.map((id) => ({ id, label: retailerLabel(id) }))
			: RETAILERS
	);

	// Re-derive the live stores whenever the route id changes. Categories and
	// store orders are list-independent (household-wide), but deriving them here
	// keeps every subscription scoped to this page's lifecycle.
	const entries = $derived(shoppingEntries(listId));
	const pendingIds = $derived(pendingEntityIds(listId));
	const cats = $derived(categories());
	const subs = $derived(subcategories());
	const orders = $derived(storeCategoryOrders());

	let selectedStore = $state<RetailerId>(loadSelectedStore());

	/**
	 * The store actually in focus. With a trip, a previously-saved store that is
	 * not in the plan falls back to the plan's first store, so the page never shows
	 * an empty list the shopper cannot leave.
	 */
	const activeStore = $derived(
		trip && !trip.allocation.stores.includes(selectedStore)
			? (trip.allocation.stores[0] ?? selectedStore)
			: selectedStore
	);

	/**
	 * The trip's allocated entry ids for the active store, or `null` when no trip
	 * is chosen (then every entry is shown, the pre-#104 behaviour).
	 */
	const storeEntryIds = $derived.by(() => {
		if (!trip) return null;
		const alloc = trip.allocation.byStore.find((s) => s.retailer === activeStore);
		return new Set((alloc?.lines ?? []).map((l) => l.entryId));
	});
	const notInStoreListCount = $derived.by(() => {
		if (!trip) return 0;
		const unallocatedIds = new Set(trip.allocation.unallocated.map((item) => item.entryId));
		const accountedIds = new Set(trip.allocation.entryIds);
		return $entries.filter(
			(entry) => unallocatedIds.has(entry.id) || !accountedIds.has(entry.id)
		).length;
	});

	/** The entries in scope for the active store (all of them without a trip). */
	const visibleEntries = $derived(
		storeEntryIds ? $entries.filter((e) => storeEntryIds.has(e.id)) : $entries
	);

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
		sortByStoreOrder(visibleEntries, $orders, activeStore, $cats, $subs).filter(
			(g) => g.remaining > 0
		)
	);

	const total = $derived(visibleEntries.length);
	const bought = $derived(visibleEntries.filter((e) => e.status === 'bought').length);

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

	<StoreSelector stores={storeOptions} selected={activeStore} onselect={selectStore} />

	{#if trip}
	<p class="trip-note">
		Shopping your {retailerLabel(activeStore)} trip — only that store’s items are shown.
		<a class="back" href="/list/{listId}/trip">
			{#if notInStoreListCount > 0}
				{notInStoreListCount} item{notInStoreListCount === 1 ? '' : 's'} not in a store list —
			{/if}
			View whole trip
		</a>
	</p>
{/if}

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
	.trip-note {
		margin: 0;
		font-size: 0.85rem;
		color: #64748b;
	}
	.empty {
		padding: 1.5rem;
		text-align: center;
		color: #94a3b8;
	}
</style>
