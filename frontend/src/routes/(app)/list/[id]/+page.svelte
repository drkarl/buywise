<script lang="ts">
	// Main list view. Renders entirely off Dexie live queries (offline-first):
	// every action below enqueues a mutation AND optimistically updates the local
	// record via the store, so the UI updates immediately without any network.
	//
	// SSR/prerender are already disabled globally in `src/routes/+layout.ts`, so
	// this dynamic `[id]` route needs no per-route prerender override.

	import { page } from '$app/state';
	import AddItemInput from '$lib/components/AddItemInput.svelte';
	import BulkAddModal from '$lib/components/BulkAddModal.svelte';
	import ListItem from '$lib/components/ListItem.svelte';
	import SyncIndicator from '$lib/components/SyncIndicator.svelte';
	import AmbiguityModal, {
		type CandidateItem
	} from '$lib/components/AmbiguityModal.svelte';
	import {
		activeEntries,
		basketEntries,
		pendingEntityIds,
		addItem,
		addItems,
		markBought,
		markSkipped,
		moveToList,
		deleteItem,
		setQuantity,
		resolveAlias
	} from '$lib/stores/list';
	import { db, type LocalListEntry } from '$lib/db/dexie';
	import {
		startSync,
		deriveSyncStatus,
		type SyncHandle,
		type FailedMutation,
		type SyncStatus
	} from '$lib/sync';

	const listId = $derived(page.params.id ?? '');

	// Re-derive the live stores whenever the route id changes.
	const active = $derived(activeEntries(listId));
	const basket = $derived(basketEntries(listId));
	// Entries whose latest edit the server has not confirmed yet — drives the
	// per-item "not synced yet" marker.
	const pendingIds = $derived(pendingEntityIds(listId));

	// Live sync/connectivity status for the always-visible indicator. Replaced by
	// the sync engine's own store on mount.
	let syncStatus = $state<SyncStatus>(deriveSyncStatus());

	let basketOpen = $state(false);
	let refreshing = $state(false);

	// Bulk paste flow (issue #115): the modal parses pasted lines into editable
	// proposals and, on confirm, queues the accepted batch through `addItems`.
	let bulkOpen = $state(false);

	// Server-rejected mutations surfaced by the sync engine. runSync has already
	// reconciled the optimistic local state away/reverted for these, so the notice
	// relies on the failure's own fields (operation + message), not a row lookup.
	let failures = $state<FailedMutation[]>([]);

	/** Human-readable verb for the rejected operation. */
	function describeOperation(op: string): string {
		switch (op) {
			case 'add_item':
				return 'add';
			case 'mark_bought':
				return 'check off';
			case 'mark_skipped':
				return 'skip';
			case 'move_to_list':
				return 'move back to the list';
			case 'delete_item':
				return 'delete';
			default:
				return op;
		}
	}

	function dismissFailure(clientMutationId: string) {
		failures = failures.filter((f) => f.client_mutation_id !== clientMutationId);
		// Acknowledge it in the sync engine too so the indicator's error state
		// clears along with the notice.
		sync?.acknowledgeFailure(clientMutationId);
	}

	// Disambiguation flow (P3-T3). The active entry being clarified and its
	// candidate items (resolved to labels off the cached catalogue), or null when
	// the modal is closed.
	let clarifying = $state<{ entryId: string; rawText: string; candidates: CandidateItem[] } | null>(
		null
	);

	/** Parse an entry's candidate ids from its server-sent JSON array, robustly. */
	function candidateIds(entry: LocalListEntry): string[] {
		if (!entry.resolution_candidates_json) return [];
		try {
			const parsed = JSON.parse(entry.resolution_candidates_json);
			return Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === 'string') : [];
		} catch {
			return [];
		}
	}

	/**
	 * Open the disambiguation modal for an ambiguous entry. Resolves each candidate
	 * id to a label from the cached catalogue (`item_categories.name_en`); a cache
	 * miss falls back to the id so the user can still pick. Reads the entry fresh
	 * from Dexie to avoid acting on a stale snapshot.
	 */
	async function openClarify(entryId: string) {
		const entry = await db.list_entries.get(entryId);
		if (!entry || entry.resolution_status !== 'ambiguous') return;
		const ids = candidateIds(entry);
		// Never open with zero candidates: the modal's Confirm stays permanently
		// disabled (nothing to pick), so it would be a dead-end the user can only
		// dismiss. An ambiguous entry with no candidates is a data anomaly we simply
		// skip rather than surfacing an unusable modal.
		if (ids.length === 0) return;
		const candidates: CandidateItem[] = await Promise.all(
			ids.map(async (id) => {
				const mapping = await db.item_categories.get(id);
				return { id, label: mapping?.name_en ?? id };
			})
		);
		clarifying = { entryId, rawText: entry.raw_text, candidates };
	}

	async function confirmClarify(chosenCanonicalItemId: string) {
		if (!clarifying) return;
		await resolveAlias(listId, clarifying.entryId, chosenCanonicalItemId);
		clarifying = null;
	}

	// Start the sync engine (queue drain + live SSE) for the current list and
	// tear it down when the id changes or the view unmounts. `$effect` re-runs
	// whenever `listId` changes; its cleanup stops the previous handle.
	let sync: SyncHandle | null = null;
	$effect(() => {
		if (!listId) return;
		failures = []; // clear notices when switching lists
		const handle = startSync(listId, {
			onFailed: (failure) => {
				// De-dupe by client_mutation_id (a replayed drain could re-report it).
				failures = [
					...failures.filter((f) => f.client_mutation_id !== failure.client_mutation_id),
					failure
				];
			}
		});
		const unsubscribeStatus = handle.status.subscribe((next) => (syncStatus = next));
		sync = handle;
		return () => {
			unsubscribeStatus();
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
			<SyncIndicator status={syncStatus} />
			<span class="count" aria-live="polite">{$active.length} item{$active.length === 1 ? '' : 's'}</span>
			<!-- Navigate to the optimise route, which fires POST /api/optimise on
			     mount and renders the plan cards. Kept as a plain link so the list
			     view stays offline-first (no network work happens here). -->
			<a class="optimise" href="/list/{listId}/optimise">Optimise basket</a>
			<button class="refresh" onclick={onRefresh} disabled={refreshing} aria-label="Refresh"
				>{refreshing ? '…' : '↻'}</button
			>
		</div>
	</header>

	{#if failures.length > 0}
		<div class="failures" role="alert" aria-live="assertive">
			{#each failures as failure (failure.client_mutation_id)}
				<div class="failure">
					<span class="failure-text"
						>Couldn’t {describeOperation(failure.operation_type)} this item: {failure.message}</span
					>
					<button
						class="failure-dismiss"
						aria-label="Dismiss"
						onclick={() => dismissFailure(failure.client_mutation_id)}>×</button
					>
				</div>
			{/each}
		</div>
	{/if}

	<div class="add-tools">
		<AddItemInput
			activeItems={$active}
			onadd={async (text) => {
				await addItem(listId, text);
			}}
		/>
		<button type="button" class="paste-list" onclick={() => (bulkOpen = true)}>
			Paste a list
		</button>
	</div>

	<ul class="entries">
		{#each $active as entry (entry.id)}
			<ListItem
				{entry}
				pending={$pendingIds.has(entry.id)}
				onbought={(id) => markBought(listId, id)}
				onskipped={(id) => markSkipped(listId, id)}
				ondelete={(id) => deleteItem(listId, id)}
				onclarify={(id) => openClarify(id)}
				onquantity={(id, quantity) => setQuantity(listId, id, quantity)}
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
							pending={$pendingIds.has(entry.id)}
							onbought={(id) => markBought(listId, id)}
							onskipped={(id) => markSkipped(listId, id)}
							ondelete={(id) => deleteItem(listId, id)}
							onmoveback={(id) => moveToList(listId, id)}
						/>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}

	{#if clarifying}
		<AmbiguityModal
			rawText={clarifying.rawText}
			candidates={clarifying.candidates}
			onconfirm={confirmClarify}
			onkeep={() => (clarifying = null)}
		/>
	{/if}

	{#if bulkOpen}
		<BulkAddModal
			activeItems={$active}
			onconfirm={async (items) => {
				await addItems(listId, items);
				bulkOpen = false;
			}}
			oncancel={() => (bulkOpen = false)}
		/>
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
	.add-tools {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.paste-list {
		align-self: flex-start;
		font-size: 0.85rem;
		font-weight: 600;
		color: #0f766e;
		background: transparent;
		border: 1px solid #0f766e;
		border-radius: 0.4rem;
		padding: 0.3rem 0.6rem;
	}
	.list-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.list-header h1 {
		font-size: 1.4rem;
		margin: 0;
	}
	.header-meta {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		flex-wrap: wrap;
		justify-content: flex-end;
	}
	.count {
		color: #64748b;
		font-size: 0.9rem;
	}
	.optimise {
		font-size: 0.85rem;
		font-weight: 600;
		color: #0f766e;
		border: 1px solid #0f766e;
		border-radius: 0.4rem;
		padding: 0.3rem 0.6rem;
		text-decoration: none;
	}
	.refresh {
		border: 1px solid #cbd5e1;
		background: #f8fafc;
		border-radius: 0.4rem;
		width: 2rem;
		height: 2rem;
		cursor: pointer;
	}
	.failures {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.failure {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 0.5rem;
		padding: 0.6rem 0.75rem;
		background: #fef2f2;
		border: 1px solid #fecaca;
		border-radius: 0.5rem;
		color: #991b1b;
		font-size: 0.9rem;
	}
	.failure-dismiss {
		border: none;
		background: transparent;
		color: #991b1b;
		font-size: 1.1rem;
		line-height: 1;
		cursor: pointer;
		padding: 0 0.25rem;
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
