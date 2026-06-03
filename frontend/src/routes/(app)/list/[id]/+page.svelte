<script lang="ts">
	// Main list view. Renders entirely off Dexie live queries (offline-first):
	// every action below enqueues a mutation AND optimistically updates the local
	// record via the store, so the UI updates immediately without any network.
	//
	// SSR/prerender are already disabled globally in `src/routes/+layout.ts`, so
	// this dynamic `[id]` route needs no per-route prerender override.

	import { page } from '$app/state';
	import AddItemInput from '$lib/components/AddItemInput.svelte';
	import ListItem from '$lib/components/ListItem.svelte';
	import {
		activeEntries,
		basketEntries,
		addItem,
		markBought,
		markSkipped,
		deleteItem
	} from '$lib/stores/list';
	import { startSync, type SyncHandle } from '$lib/sync';

	const listId = $derived(page.params.id ?? '');

	// Re-derive the live stores whenever the route id changes.
	const active = $derived(activeEntries(listId));
	const basket = $derived(basketEntries(listId));

	let basketOpen = $state(false);
	let refreshing = $state(false);

	// Start the sync engine (queue drain + live SSE) for the current list and
	// tear it down when the id changes or the view unmounts. `$effect` re-runs
	// whenever `listId` changes; its cleanup stops the previous handle.
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

	async function onRefresh() {
		refreshing = true;
		try {
			await sync?.resync();
		} finally {
			refreshing = false;
		}
	}
</script>

<svelte:head>
	<title>BuyWise — List</title>
</svelte:head>

<main class="list-view">
	<header class="list-header">
		<h1>Shopping list</h1>
		<div class="header-meta">
			<span class="count" aria-live="polite">{$active.length} item{$active.length === 1 ? '' : 's'}</span>
			<button class="refresh" onclick={onRefresh} disabled={refreshing} aria-label="Refresh"
				>{refreshing ? '…' : '↻'}</button
			>
		</div>
	</header>

	<AddItemInput
		onadd={async (text) => {
			await addItem(listId, text);
		}}
	/>

	<ul class="entries">
		{#each $active as entry (entry.id)}
			<ListItem
				{entry}
				onbought={(id) => markBought(listId, id)}
				onskipped={(id) => markSkipped(listId, id)}
				ondelete={(id) => deleteItem(listId, id)}
			/>
		{:else}
			<li class="empty">Nothing here yet — add your first item above.</li>
		{/each}
	</ul>

	{#if $basket.length > 0}
		<section class="basket">
			<button
				class="basket-toggle"
				aria-expanded={basketOpen}
				onclick={() => (basketOpen = !basketOpen)}
			>
				{basketOpen ? '▾' : '▸'} In basket ({$basket.length})
			</button>
			{#if basketOpen}
				<ul class="entries">
					{#each $basket as entry (entry.id)}
						<ListItem
							{entry}
							onbought={(id) => markBought(listId, id)}
							onskipped={(id) => markSkipped(listId, id)}
							ondelete={(id) => deleteItem(listId, id)}
						/>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}
</main>

<style>
	.list-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.list-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.list-header h1 {
		font-size: 1.4rem;
		margin: 0;
	}
	.header-meta {
		display: flex;
		align-items: center;
		gap: 0.75rem;
	}
	.count {
		color: #64748b;
		font-size: 0.9rem;
	}
	.refresh {
		border: 1px solid #cbd5e1;
		background: #f8fafc;
		border-radius: 0.4rem;
		width: 2rem;
		height: 2rem;
		cursor: pointer;
	}
	.entries {
		margin: 0;
		padding: 0;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		overflow: hidden;
	}
	.empty {
		list-style: none;
		padding: 1rem;
		color: #94a3b8;
		text-align: center;
	}
	.basket-toggle {
		width: 100%;
		text-align: left;
		padding: 0.6rem 0.75rem;
		font-weight: 600;
		background: #f1f5f9;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		cursor: pointer;
	}
	.basket .entries {
		margin-top: 0.5rem;
	}
</style>
