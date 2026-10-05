<script lang="ts">
	// Per-store trip view of the chosen plan (issue #104). Renders the household's
	// chosen optimiser plan as one shopping list per store — each grouped by the
	// store's own category order — plus an explicit "not allocated" section for
	// items the plan could not price. Everything derives from Dexie live queries,
	// so it works fully offline: the allocation is a VIEW over the same shared list
	// entries, and check-offs reuse the normal `markBought` mutation (optimistic +
	// queued), so nothing here is a separate copy that can drift.
	//
	// The chosen plan itself is a client-side cache (`chosenTrip.ts`), read on
	// mount. When the list has changed since the plan was chosen (an entry added or
	// removed) a banner offers the review path back to the optimiser.
	//
	// SSR/prerender are disabled globally in `src/routes/+layout.ts`, so this
	// dynamic `[id]` route needs no per-route override.

	import { page } from '$app/state';
	import CategoryGroup from '$lib/components/CategoryGroup.svelte';
	import SyncIndicator from '$lib/components/SyncIndicator.svelte';
	import { markBought, pendingEntityIds } from '$lib/stores/list';
	import {
		shoppingEntries,
		allEntries,
		categories,
		subcategories,
		storeCategoryOrders
	} from '$lib/stores/shopping';
	import { sortByStoreOrder } from '$lib/shopping/sortByStoreOrder';
	import { loadChosenTrip, isTripStale } from '$lib/shopping/chosenTrip';
	import { type AllocatedLine, type UnallocatedReason } from '$lib/shopping/allocatePlan';
	import { formatEuros, retailerLabel } from '$lib/api/optimise';
	import { startSync, deriveSyncStatus, type SyncHandle, type SyncStatus } from '$lib/sync';

	const listId = $derived(page.params.id ?? '');

	/** The chosen plan, read from localStorage; re-read if the route id changes. */
	const trip = $derived(loadChosenTrip(listId));

	// Live views over the shared list entries and the cached catalogue.
	const entries = $derived(shoppingEntries(listId));
	const all = $derived(allEntries(listId));
	const pendingIds = $derived(pendingEntityIds(listId));
	const cats = $derived(categories());
	const subs = $derived(subcategories());
	const orders = $derived(storeCategoryOrders());

	/** True when the list no longer matches the plan (an entry added or removed). */
	const stale = $derived(trip ? isTripStale(trip, $all) : false);

	/**
	 * One view per store in the chosen plan: the store's entries grouped by its
	 * category order, the chosen product per entry, and the plan's estimated
	 * spend at that store. Sections with no items left to buy are hidden, matching
	 * in-store shopping mode.
	 */
	const storeViews = $derived(
		!trip
			? []
			: trip.allocation.byStore.map((alloc) => {
					const lineByEntry: Record<string, AllocatedLine> = {};
					for (const line of alloc.lines) lineByEntry[line.entryId] = line;
					const items = $entries.filter((e) => lineByEntry[e.id] !== undefined);
					const products: Record<string, string> = {};
					for (const [entryId, line] of Object.entries(lineByEntry)) {
						products[entryId] = line.productName;
					}
					return {
						retailer: alloc.retailer,
						groups: sortByStoreOrder(items, $orders, alloc.retailer, $cats, $subs).filter(
							(g) => g.remaining > 0
						),
						products,
						totalCents: alloc.lines.reduce((sum, l) => sum + l.lineTotalCents, 0)
					};
				})
	);

	/** The explicit unallocated section: its entries grouped by category + reason. */
	const unallocatedView = $derived(
		!trip
			? null
			: (() => {
					const reasons: Record<string, UnallocatedReason> = {};
					for (const item of trip.allocation.unallocated) reasons[item.entryId] = item.reason;
					const items = $entries.filter((e) => reasons[e.id] !== undefined);
					// Group by the first planned store's aisle order (any store works —
					// the section is store-independent).
					const store = trip.allocation.stores[0] ?? 'tesco';
					return {
						groups: sortByStoreOrder(items, $orders, store, $cats, $subs),
						reasons,
						count: items.length
					};
				})()
	);

	/** Human-readable reason an item sits in the unallocated section. */
	function reasonText(reason: UnallocatedReason): string {
		switch (reason) {
			case 'unresolved':
				return 'needs clarifying';
			case 'no_offer':
				return 'no matching product';
			case 'needs_confirmation':
				return 'waiting for your confirmation';
			case 'not_in_plan':
				return 'not stocked at your stores';
		}
	}

	// Start the sync engine so check-offs flush when online and remote changes
	// stream in; identical lifecycle to the main list and shopping views.
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
	<title>BuyWise — Trip</title>
</svelte:head>

<main class="trip-view">
	<header class="trip-header">
		<div>
			<h1>{trip?.title ?? 'Trip'}</h1>
			{#if trip}
				<p class="trip-stores">{trip.allocation.stores.map(retailerLabel).join(' + ')}</p>
			{/if}
		</div>
		<div class="trip-header-meta">
			<SyncIndicator status={syncStatus} ondismiss={() => void sync?.dismissFailures()} />
			<a class="link" href="/list/{listId}">Back to list</a>
			<a class="link" href="/list/{listId}/optimise">Change plan</a>
		</div>
	</header>

	{#if !trip}
		<p class="empty">
			No plan selected yet. <a class="link" href="/list/{listId}/optimise">Choose a plan</a> to see
			your per-store lists.
		</p>
	{:else}
		{#if stale}
			<p class="stale" role="alert">
				Your list has changed since you chose this plan, so it may not cover everything below.
				<a class="link" href="/list/{listId}/optimise">Review and choose a plan again</a>.
			</p>
		{/if}

		{#each storeViews as view (view.retailer)}
			<section class="store-trip" aria-label="{retailerLabel(view.retailer)} trip">
				<header class="store-header">
					<h2>{retailerLabel(view.retailer)} trip</h2>
					<span class="store-total">Estimated {formatEuros(view.totalCents)}</span>
				</header>
				{#if view.groups.length > 0}
					<div class="groups">
						{#each view.groups as group (group.id)}
							<CategoryGroup
								{group}
								products={view.products}
								pendingIds={$pendingIds}
								onbought={(id) => markBought(listId, id)}
							/>
						{/each}
					</div>
				{:else}
					<p class="empty">All done! Every item for this store is checked off.</p>
				{/if}
			</section>
		{/each}

		{#if unallocatedView}
			<section class="unallocated" aria-label="Not allocated to a store">
				<header class="store-header">
					<h2>Not allocated to a store</h2>
					<span class="store-total">{unallocatedView.count} item{unallocatedView.count === 1 ? '' : 's'}</span>
				</header>
				<p class="note">
					These items aren’t in a store’s list — they need clarifying, have no matching product, are
					waiting for your confirmation, or aren’t stocked at your stores. Check them off here when
					you pick them up.
				</p>
				{#if unallocatedView.count > 0}
					<div class="groups">
						{#each unallocatedView.groups as group (group.id)}
							<CategoryGroup
								{group}
								pendingIds={$pendingIds}
								onbought={(id) => markBought(listId, id)}
							/>
						{/each}
					</div>
					<ul class="reasons">
						{#each unallocatedView.groups.flatMap((g) => g.items) as item (item.id)}
							<li>{item.display_name} — {reasonText(unallocatedView.reasons[item.id])}</li>
						{/each}
					</ul>
				{:else}
					<p class="empty">Nothing unallocated — this plan covers your whole list.</p>
				{/if}
			</section>
		{/if}
	{/if}
</main>

<style>
	.trip-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.trip-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.trip-header h1 {
		font-size: 1.4rem;
		margin: 0;
	}
	.trip-stores {
		margin: 0;
		color: #475569;
		font-size: 0.9rem;
	}
	.trip-header-meta {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		flex-wrap: wrap;
		justify-content: flex-end;
	}
	.link {
		font-size: 0.9rem;
		color: #0f766e;
	}
	.stale {
		margin: 0;
		padding: 0.75rem;
		background: #fffbeb;
		border: 1px solid #fde68a;
		border-radius: 0.5rem;
		color: #92400e;
		font-size: 0.9rem;
	}
	.store-trip {
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
	}
	.store-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.store-header h2 {
		font-size: 1.15rem;
		margin: 0;
	}
	.store-total {
		font-size: 0.85rem;
		color: #64748b;
		font-variant-numeric: tabular-nums;
	}
	.groups {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.unallocated {
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		border-top: 2px solid #e2e8f0;
		padding-top: 1rem;
	}
	.note {
		margin: 0;
		font-size: 0.85rem;
		color: #64748b;
	}
	.reasons {
		margin: 0;
		padding-left: 1.1rem;
		font-size: 0.82rem;
		color: #64748b;
	}
	.empty {
		padding: 1.5rem;
		text-align: center;
		color: #94a3b8;
	}
</style>
