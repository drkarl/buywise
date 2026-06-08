<script lang="ts">
	// Basket optimisation view (P4-T3). Unlike the offline-first list/shopping
	// views, this page is intrinsically online: it asks the server to cost the
	// resolved basket across retailers. On mount it first flushes any pending
	// local edits to the server (list edits are offline-first and only ENQUEUE
	// locally — see lib/stores/list.ts + lib/sync), because navigating here can
	// outrun the list view's enqueue-driven drain and we must not price a stale
	// server list. If the flush can't confirm every edit (offline) we surface an
	// error and skip pricing. Only once synced does it fire the CSRF-protected
	// `POST /api/optimise`, show a loading state while the server computes, then
	// render the returned plans.
	//
	// SSR/prerender are disabled globally in `src/routes/+layout.ts`, so this
	// dynamic `[id]` route needs no per-route override; the on-mount fetch only
	// runs in the browser regardless.
	//
	// The optimiser returns up to four plan shapes (best single, best two-store,
	// theoretical minimum, and a routed recommendation); any may be null. We render
	// only the non-null ones and visually highlight `recommended`. Items the
	// optimiser couldn't price are surfaced separately so they never silently skew
	// a total — unresolved items link back to the list view's existing clarify
	// flow (there is no separate clarify route).

	import { page } from '$app/state';
	import { jsonMutationHeaders } from '$lib/api/csrf';
	import { flushPendingMutations } from '$lib/sync';
	import PlanCard from '$lib/components/PlanCard.svelte';
	import type { NoValidOfferReason, OptimiseResponse } from '$lib/api/optimise';

	const listId = $derived(page.params.id ?? '');

	type Status = 'loading' | 'ready' | 'error';

	let status = $state<Status>('loading');
	let result = $state<OptimiseResponse | null>(null);
	let errorMessage = $state('');

	/**
	 * The single-store baseline total used for the OTHER cards' "saving vs" line.
	 * The best-single card itself is passed `null` so it doesn't self-compare and
	 * render a redundant "Same price as best single store" line.
	 */
	const bestSingleTotal = $derived(result?.best_single?.total_cents ?? null);

	/**
	 * POST the current list to the optimiser. On a non-OK response we surface a
	 * visible error rather than failing silently; the message stays generic (status
	 * code only) because the endpoint's error bodies aren't a stable contract.
	 *
	 * Guards against a stale response: if `listId` changes mid-request (the route
	 * id navigated to another list while the POST was in flight), we drop the
	 * resolved response instead of rendering it against the wrong list.
	 *
	 * Pre-flush: list edits are enqueued locally and synced asynchronously, so we
	 * flush the pending queue first and refuse to price unless every edit reached
	 * the server (`synced`). An unconfirmed edit (offline) means the server list
	 * is still stale; we error out rather than show a plan that omits it. `retry()`
	 * re-runs this, so reconnect → retry covers recovery.
	 */
	async function runOptimise(id: string) {
		status = 'loading';
		errorMessage = '';
		try {
			const { synced } = await flushPendingMutations(id);
			// Ignore a flush for a list we're no longer viewing.
			if (id !== listId) return;
			if (!synced) {
				errorMessage =
					'Some changes haven’t been saved to the server yet. Reconnect and try again so your plan covers your whole list.';
				status = 'error';
				return;
			}

			const res = await fetch('/api/optimise', {
				method: 'POST',
				headers: jsonMutationHeaders(),
				body: JSON.stringify({ list_id: id })
			});
			// Ignore a response for a list we're no longer viewing.
			if (id !== listId) return;
			if (!res.ok) {
				errorMessage = `Couldn’t optimise your basket (error ${res.status}). Please try again.`;
				status = 'error';
				return;
			}
			const body = (await res.json()) as OptimiseResponse;
			// Re-check after the async body read in case the id changed meanwhile.
			if (id !== listId) return;
			result = body;
			status = 'ready';
		} catch {
			if (id !== listId) return;
			errorMessage = 'Couldn’t reach the server. Check your connection and try again.';
			status = 'error';
		}
	}

	// Run once per list id. `$effect` re-runs if the route id changes (rare here)
	// so navigating between lists re-optimises the right basket.
	$effect(() => {
		if (!listId) return;
		runOptimise(listId);
	});

	function retry() {
		if (listId) runOptimise(listId);
	}

	/**
	 * Reason copy for an item that resolved but yielded no usable offer. Typed to
	 * the `NoValidOfferReason` union so the switch stays exhaustive if reasons are
	 * added; the `default` is a defensive fallback for an unknown wire value.
	 */
	function reasonText(reason: NoValidOfferReason): string {
		switch (reason) {
			case 'no_matches':
				return 'no matching product found';
			case 'constraints_eliminated_all':
				return 'your constraints ruled out every option';
			case 'brand_policy_missing_brand':
				return 'a brand is required but none was set';
			default:
				return reason;
		}
	}
</script>

<svelte:head>
	<title>BuyWise — Optimise basket</title>
</svelte:head>

<main class="optimise-view">
	<header class="optimise-header">
		<h1>Optimise basket</h1>
		<a class="back" href="/list/{listId}">Back to list</a>
	</header>

	{#if status === 'loading'}
		<p class="loading" role="status" aria-live="polite">Costing your basket across stores…</p>
	{:else if status === 'error'}
		<div class="error" role="alert">
			<p>{errorMessage}</p>
			<button type="button" class="retry" onclick={retry}>Try again</button>
		</div>
	{:else if result}
		{#if result.offer_snapshot_date}
			<p class="snapshot">Prices as of {result.offer_snapshot_date}.</p>
		{/if}

		<section class="plans" aria-label="Plans">
			{#if result.recommended}
				<PlanCard
					plan={result.recommended.plan}
					title={result.recommended.kind === 'two_store' ? 'Recommended split' : 'Recommended store'}
					recommended
					bestSingleTotalCents={bestSingleTotal}
				/>
			{/if}
			{#if result.best_single}
				<PlanCard
					plan={result.best_single}
					title="Best single store"
					bestSingleTotalCents={null}
				/>
			{/if}
			{#if result.best_two_store}
				<PlanCard
					plan={result.best_two_store}
					title="Cheapest two-store split"
					bestSingleTotalCents={bestSingleTotal}
				/>
			{/if}
			{#if result.theoretical_minimum}
				<PlanCard
					plan={result.theoretical_minimum}
					title="Theoretical minimum"
					benchmark
					bestSingleTotalCents={bestSingleTotal}
				/>
			{/if}
			{#if !result.recommended && !result.best_single && !result.best_two_store && !result.theoretical_minimum}
				<p class="empty">No plans to show — add some items to your list, then optimise again.</p>
			{/if}
		</section>

		{#if result.plain_language_explanation}
			<section class="explanation" aria-label="Explanation">
				<h2>What this means</h2>
				<p>{result.plain_language_explanation}</p>
			</section>
		{/if}

		{#if result.unresolved_items.length > 0 || result.no_valid_offer_items.length > 0}
			<section class="unresolved" aria-label="Unresolved items">
				<h2>Items excluded from totals</h2>
				<p class="unresolved-note">
					These items couldn’t be priced, so they’re not included in any plan total.
				</p>
				<ul>
					{#each result.unresolved_items as item (item.item_index)}
						<li>
							<span>{item.display_text} — needs clarifying</span>
							<a class="clarify" href="/list/{listId}">Clarify on your list</a>
						</li>
					{/each}
					{#each result.no_valid_offer_items as item (item.item_index)}
						<li>
							<span>{item.display_text} — {reasonText(item.reason)}</span>
							<a class="clarify" href="/list/{listId}">Review on your list</a>
						</li>
					{/each}
				</ul>
			</section>
		{/if}
	{/if}
</main>

<style>
	.optimise-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.optimise-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.optimise-header h1 {
		font-size: 1.4rem;
		margin: 0;
	}
	.back {
		font-size: 0.9rem;
		color: #0f766e;
	}
	.loading {
		padding: 1.5rem;
		text-align: center;
		color: #64748b;
	}
	.error {
		padding: 1rem;
		background: #fef2f2;
		border: 1px solid #fecaca;
		border-radius: 0.5rem;
		color: #991b1b;
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		align-items: flex-start;
	}
	.error p {
		margin: 0;
	}
	.retry {
		padding: 0.4rem 0.9rem;
		border-radius: 0.5rem;
		border: 1px solid #991b1b;
		background: #fff;
		color: #991b1b;
		cursor: pointer;
	}
	.snapshot {
		margin: 0;
		font-size: 0.85rem;
		color: #94a3b8;
	}
	.plans {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.explanation,
	.unresolved {
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 1rem;
	}
	.explanation h2,
	.unresolved h2 {
		margin: 0 0 0.5rem;
		font-size: 1.05rem;
	}
	.explanation p {
		margin: 0;
		color: #334155;
		line-height: 1.5;
	}
	.unresolved-note {
		margin: 0 0 0.6rem;
		color: #64748b;
		font-size: 0.9rem;
	}
	.unresolved ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.unresolved li {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		font-size: 0.92rem;
	}
	.clarify {
		color: #0f766e;
		white-space: nowrap;
		flex-shrink: 0;
	}
	.empty {
		padding: 1.5rem;
		text-align: center;
		color: #94a3b8;
	}
</style>
