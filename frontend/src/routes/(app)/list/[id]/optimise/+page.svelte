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
	// a total — they link back to the list view (there is no separate clarify
	// route). The link says "Review on your list" rather than promising a clarify
	// control: the response carries no resolution_status, so we can't tell an
	// `ambiguous` row (which has a clarify control) from a plain `unresolved` one
	// (which doesn't).

	import { page } from '$app/state';
	import { jsonMutationHeaders } from '$lib/api/csrf';
	import { fetchSettings } from '$lib/api/settings';
	import { flushPendingMutations } from '$lib/sync';
	import PlanCard from '$lib/components/PlanCard.svelte';
	import { allocatePlan } from '$lib/shopping/allocatePlan';
	import { saveChosenTrip } from '$lib/shopping/chosenTrip';
	import type { RetailerId } from '$lib/stores/shopping';
	import {
		formatEuros,
		retailerLabel,
		sendMatchFeedback,
		type AldiComparison,
		type AldiComparisonSplit,
		type CandidateOffer,
		type NoValidOfferReason,
		type OptimiseResponse,
		type PlanKind,
		type StorePlan
	} from '$lib/api/optimise';

	const listId = $derived(page.params.id ?? '');

	type Status = 'loading' | 'ready' | 'error';

	let status = $state<Status>('loading');
	let result = $state<OptimiseResponse | null>(null);
	let errorMessage = $state('');

	/**
	 * The `canonical_item_id`s whose `prefer` POST is currently in flight. A set
	 * (not a single id) because the UI lets several ask items be confirmed
	 * concurrently — each item disables only its own buttons, so a second
	 * confirmation must not overwrite or clear the first's busy state. Keyed by
	 * canonical id rather than item_index so the disabled state survives the
	 * re-optimise that replaces `result`.
	 */
	let confirmingItemIds = $state<Set<string>>(new Set());
	/** A per-item confirmation error message, keyed by canonical id. */
	let confirmError = $state<Record<string, string>>({});

	/**
	 * The household's `occasional` stores (issue #98). Loaded from settings so the
	 * page can offer "are you going there today?" toggles; a store not marked is
	 * left out of this run's plans. A settings fetch failure is non-fatal — the
	 * page simply shows no trip control (active/off stores need none).
	 */
	let occasionalStores = $state<RetailerId[]>([]);
	/** The occasional stores the household is visiting today, sent as `trip_retailers`. */
	let tripRetailers = $state<RetailerId[]>([]);

	/** Load the list of occasional stores once per mount; non-fatal on failure. */
	async function loadOccasionalStores() {
		try {
			const settings = await fetchSettings();
			occasionalStores = settings.stores
				.filter((s) => s.status === 'occasional')
				.map((s) => s.retailer);
		} catch {
			occasionalStores = [];
		}
	}

	/** Toggle whether the household is visiting `store` today, then re-optimise. */
	function toggleTrip(store: RetailerId) {
		tripRetailers = tripRetailers.includes(store)
			? tripRetailers.filter((r) => r !== store)
			: [...tripRetailers, store];
		if (listId) runOptimise(listId);
	}

	/**
	 * Confirm a candidate for an ask-gated item (issue #78): persist the choice via
	 * `POST /api/match-feedback` with action `prefer`, then re-optimise so the now-
	 * confirmed item enters the plans. On failure we surface an inline message and
	 * leave the chooser in place so the household can retry; we never silently drop
	 * the choice. `runOptimise` resets `status` to loading, so the chooser is
	 * replaced by the recomputed plans on success.
	 *
	 * Re-entrancy: ignore a click for an item already in flight (its button is
	 * disabled, but a stray double-fire must not send a duplicate `prefer`). The
	 * busy set is reassigned rather than mutated in place so the `$state` tracks it.
	 */
	async function chooseCandidate(canonicalItemId: string, candidate: CandidateOffer) {
		if (confirmingItemIds.has(canonicalItemId)) return;
		confirmingItemIds = new Set(confirmingItemIds).add(canonicalItemId);
		confirmError = { ...confirmError, [canonicalItemId]: '' };
		try {
			const { ok } = await sendMatchFeedback({
				canonicalItemId,
				retailerProductId: candidate.retailer_product_id,
				action: 'prefer'
			});
			if (!ok) {
				confirmError = {
					...confirmError,
					[canonicalItemId]: 'Couldn’t save your choice. Please try again.'
				};
				return;
			}
			await runOptimise(listId);
		} catch {
			confirmError = {
				...confirmError,
				[canonicalItemId]: 'Couldn’t reach the server. Check your connection and try again.'
			};
		} finally {
			// Clear only this item's busy flag, leaving any concurrent confirmation's
			// flag intact.
			const next = new Set(confirmingItemIds);
			next.delete(canonicalItemId);
			confirmingItemIds = next;
		}
	}

	/**
	 * The single-store baseline total used for the OTHER cards' "saving vs" line.
	 * The best-single card itself is passed `null` so it doesn't self-compare and
	 * render a redundant "Same price as best single store" line.
	 */
	const bestSingleTotal = $derived(result?.best_single?.total_cents ?? null);
	const bestSingleItemIndices = $derived(
		result?.best_single?.lines.map((line) => line.item_index) ?? []
	);

	/**
	 * The Aldi side comparison (issue #100), or `null` when there is no
	 * recommended plan. It never affects the plans; it exists so the household can
	 * judge whether Aldi is worth shopping at. `aldiCovered` is false when Aldi has
	 * no qualifying price for any item the plan prices — we show a "nothing to
	 * compare" note rather than a misleading €0.00 saving.
	 */
	const aldiComparison = $derived(result?.aldi_comparison ?? null);
	const aldiCovered = $derived(
		!!aldiComparison &&
			(aldiComparison.fresh.item_count > 0 || aldiComparison.packaged.item_count > 0)
	);

	/** Headline: Aldi's total saving across both splits, or the cost if it is pricier. */
	function aldiSummary(comparison: AldiComparison): string {
		const total = comparison.fresh.saving_cents + comparison.packaged.saving_cents;
		if (total >= 0) {
			return `Aldi would have saved ${formatEuros(total)} on this list.`;
		}
		return `Aldi would have cost ${formatEuros(Math.abs(total))} more than your plan.`;
	}

	/** One split's line: the saving (or extra cost) and the two totals it compares. */
	function aldiSplitLine(label: string, split: AldiComparisonSplit): string {
		const totals = `Aldi ${formatEuros(split.aldi_total_cents)} vs your plan ${formatEuros(
			split.recommended_total_cents
		)}`;
		if (split.saving_cents >= 0) {
			return `${label}: ${formatEuros(split.saving_cents)} cheaper. ${totals} for ${split.item_count} item${
				split.item_count === 1 ? '' : 's'
			}.`;
		}
		return `${label}: ${formatEuros(Math.abs(split.saving_cents))} more. ${totals} for ${
			split.item_count
		} item${split.item_count === 1 ? '' : 's'}.`;
	}

	/**
	 * Plain-language reasons for the card of a given plan kind (issue #103). The
	 * optimiser may omit a plan entirely or send it with no reasons, so both fall
	 * back to an empty list and the card renders nothing extra.
	 */
	function planReasons(kind: PlanKind): string[] {
		return result?.plan_explanations.find((p) => p.plan === kind)?.reasons ?? [];
	}

	/** Card title for the recommended plan, reused as the chosen trip's label. */
	const recommendedTitle = $derived(
		result?.recommended?.kind === 'two_store' ? 'Recommended split' : 'Recommended store'
	);

	/**
	 * The trip link for a routable plan, or `null` when the plan cannot back a
	 * per-store trip. Only single- and two-store plans qualify (three-store routing
	 * is a non-goal), and a plan that priced nothing has no list to show.
	 */
	function tripHref(plan: StorePlan | null | undefined): string | null {
		if (!plan || !listId) return null;
		if (plan.stores.length === 0 || plan.stores.length > 2 || plan.lines.length === 0) return null;
		return `/list/${listId}/trip`;
	}

	/**
	 * Persist the household's choice: allocate the picked plan over the shared list
	 * entries (issue #104) and cache it for the trip view and store-filtered
	 * shopping mode. Runs on the "Shop this plan" link's click, before navigation;
	 * storing is synchronous localStorage, so the trip view reads it on mount.
	 * No-op without a loaded response.
	 */
	function choosePlan(title: string, plan: StorePlan) {
		if (!result) return;
		saveChosenTrip({
			listId,
			runId: result.run_id,
			planKind: plan.stores.length > 1 ? 'two_store' : 'single_store',
			title,
			allocation: allocatePlan({
				plan,
				unresolvedItems: result.unresolved_items,
				noValidOfferItems: result.no_valid_offer_items,
				needsConfirmationItems: result.needs_confirmation_items,
				basketItems: result.basket_items
			}),
			savedAt: new Date().toISOString()
		});
	}

	/**
	 * Monotonic token for in-flight `runOptimise` calls. Each call captures the
	 * latest value; after every await it bails if a newer call has since started.
	 * This guards against overlapping *same-list* runs — e.g. confirming ask item A
	 * then B before A's re-optimise resolves — where an older response could
	 * otherwise land last and clobber the newer one (the `id !== listId` check only
	 * catches navigation to a *different* list). Not reactive: a render never reads
	 * it, it only gates which run is allowed to assign `result`/`status`.
	 */
	let optimiseRequestSeq = 0;

	/**
	 * POST the current list to the optimiser. On a non-OK response we surface a
	 * visible error rather than failing silently; the message stays generic (status
	 * code only) because the endpoint's error bodies aren't a stable contract.
	 *
	 * Guards against a stale response on two axes: if `listId` changes mid-request
	 * (navigated to another list) OR a newer `runOptimise` started (`optimiseRequestSeq`
	 * moved on, e.g. a second confirmation), we drop the resolved response instead of
	 * rendering it — only the latest run for the current list may assign state.
	 *
	 * Pre-flush: list edits are enqueued locally and synced asynchronously, so we
	 * flush the pending queue first and refuse to price unless every edit reached
	 * the server (`synced`). An unconfirmed edit (offline) means the server list
	 * is still stale; we error out rather than show a plan that omits it. `retry()`
	 * re-runs this, so reconnect → retry covers recovery.
	 */
	async function runOptimise(id: string) {
		const seq = ++optimiseRequestSeq;
		status = 'loading';
		errorMessage = '';
		// True once a newer run has superseded this one or the route navigated away;
		// such a run must not assign `result`/`status`.
		const superseded = () => id !== listId || seq !== optimiseRequestSeq;
		try {
			const { synced } = await flushPendingMutations(id);
			if (superseded()) return;
			if (!synced) {
				errorMessage =
					'Some changes haven’t been saved to the server yet. Reconnect and try again so your plan covers your whole list.';
				status = 'error';
				return;
			}

			const res = await fetch('/api/optimise', {
				method: 'POST',
				headers: jsonMutationHeaders(),
				body: JSON.stringify({ list_id: id, trip_retailers: tripRetailers })
			});
			if (superseded()) return;
			if (!res.ok) {
				errorMessage = `Couldn’t optimise your basket (error ${res.status}). Please try again.`;
				status = 'error';
				return;
			}
			const body = (await res.json()) as OptimiseResponse;
			// Re-check after the async body read in case a newer run started meanwhile.
			if (superseded()) return;
			result = body;
			status = 'ready';
		} catch {
			if (superseded()) return;
			errorMessage = 'Couldn’t reach the server. Check your connection and try again.';
			status = 'error';
		}
	}

	// Run once per list id. `$effect` re-runs if the route id changes (rare here)
	// so navigating between lists re-optimises the right basket. The occasional-
	// store list is loaded alongside so the trip control is ready.
	$effect(() => {
		if (!listId) return;
		loadOccasionalStores();
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
			case 'store_category_rules_excluded_all':
				return 'store category rules excluded every option';
			case 'brand_policy_missing_brand':
				return 'a brand is required but none was set';
			case 'strict_no_exact_offer':
				return 'the exact product has no current offer, and substitutes aren’t allowed';
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

		{#if occasionalStores.length > 0}
			<section class="trip" aria-label="Stores you are visiting today">
				<h2>Going anywhere else today?</h2>
				<p class="trip-note">
					You only visit these stores occasionally. Mark any you are going to now and they’ll be
					included in your plan.
				</p>
				<div class="trip-buttons">
					{#each occasionalStores as store (store)}
						<button
							type="button"
							class="trip-store"
							class:on={tripRetailers.includes(store)}
							aria-pressed={tripRetailers.includes(store)}
							onclick={() => toggleTrip(store)}
						>
							{retailerLabel(store)}
						</button>
					{/each}
				</div>
			</section>
		{/if}

		<section class="plans" aria-label="Plans">
			{#if result.recommended}
				<PlanCard
					plan={result.recommended.plan}
					title={recommendedTitle}
					recommended
					bestSingleTotalCents={bestSingleTotal}
					bestSingleItemIndices={bestSingleItemIndices}
					reasons={planReasons('recommended')}
					chooseHref={tripHref(result.recommended.plan)}
					onchoose={() => choosePlan(recommendedTitle, result!.recommended!.plan)}
				/>
			{/if}
			{#if result.best_single}
				<PlanCard
					plan={result.best_single}
					title="Best single store"
					bestSingleTotalCents={null}
					reasons={planReasons('best_single')}
					chooseHref={tripHref(result.best_single)}
					onchoose={() => choosePlan('Best single store', result!.best_single!)}
				/>
			{/if}
			{#if result.best_two_store}
				<PlanCard
					plan={result.best_two_store}
					title="Cheapest two-store split"
					bestSingleTotalCents={bestSingleTotal}
					bestSingleItemIndices={bestSingleItemIndices}
					reasons={planReasons('best_two_store')}
					chooseHref={tripHref(result.best_two_store)}
					onchoose={() => choosePlan('Cheapest two-store split', result!.best_two_store!)}
				/>
			{/if}
			{#if result.theoretical_minimum}
				<PlanCard
					plan={result.theoretical_minimum}
					title="Theoretical minimum"
					benchmark
					bestSingleTotalCents={bestSingleTotal}
					bestSingleItemIndices={bestSingleItemIndices}
					reasons={planReasons('theoretical_minimum')}
				/>
			{/if}
			{#if !result.recommended && !result.best_single && !result.best_two_store && !result.theoretical_minimum}
				{#if result.needs_confirmation_items.length > 0}
					<p class="empty">No plans yet — some items need confirmation or review below.</p>
				{:else}
					<p class="empty">No plans to show — add some items to your list, then optimise again.</p>
				{/if}
			{/if}
		</section>

		{#if aldiComparison}
			<section class="aldi" aria-label="Aldi comparison">
				<h2>Aldi comparison</h2>
				{#if aldiCovered}
					<p class="aldi-summary">{aldiSummary(aldiComparison)}</p>
					<ul class="aldi-splits">
						{#if aldiComparison.fresh.item_count > 0}
							<li>{aldiSplitLine('Fresh food', aldiComparison.fresh)}</li>
						{/if}
						{#if aldiComparison.packaged.item_count > 0}
							<li>{aldiSplitLine('Packaged food', aldiComparison.packaged)}</li>
						{/if}
					</ul>
				{:else}
					<p class="aldi-empty">
						No Aldi prices on record for the items in your plan, so there’s nothing to compare
						yet.
					</p>
				{/if}
				<p class="aldi-note">
					For information only — Aldi never changes which plan is recommended.
				</p>
			</section>
		{/if}

		{#if result.plain_language_explanation}
			<section class="explanation" aria-label="Explanation">
				<h2>What this means</h2>
				<p>{result.plain_language_explanation}</p>
			</section>
		{/if}

		{#if result.needs_confirmation_items.length > 0}
			<section class="needs-confirmation" aria-label="Items awaiting your confirmation">
				<h2>Waiting for your confirmation</h2>
				<p class="needs-confirmation-note">
					These items are set to ask before substituting, so they’re held out of every plan
					total. Choose a product to include it in your plan.
				</p>
				<ul class="confirm-list">
					{#each result.needs_confirmation_items as item (item.item_index)}
						<li class="confirm-item">
							<p class="confirm-item-name">{item.display_text}</p>
							<ul class="candidate-list">
								{#each item.candidates as candidate (candidate.retailer_product_id)}
									<li class="candidate">
										<span class="candidate-detail">
											<span class="candidate-name">{candidate.name}</span>
											{#if candidate.brand}<span class="candidate-meta">{candidate.brand}</span>{/if}
											{#if candidate.size_text}<span class="candidate-meta"
													>{candidate.size_text}</span
												>{/if}
											<span class="candidate-meta">{retailerLabel(candidate.retailer)}</span>
											<span class="candidate-price">{formatEuros(candidate.price_cents)}</span>
										</span>
										<button
											type="button"
											class="choose"
											disabled={confirmingItemIds.has(item.canonical_item_id)}
											aria-busy={confirmingItemIds.has(item.canonical_item_id)}
											onclick={() => chooseCandidate(item.canonical_item_id, candidate)}
										>
											Choose
										</button>
									</li>
								{/each}
							</ul>
							{#if confirmError[item.canonical_item_id]}
								<p class="confirm-error" role="alert">{confirmError[item.canonical_item_id]}</p>
							{/if}
						</li>
					{/each}
				</ul>
			</section>
		{/if}

		{#if result.unresolved_items.length > 0 || result.no_valid_offer_items.length > 0}
			<section class="unresolved" aria-label="Unresolved items">
				<h2>Items excluded from totals</h2>
				<p class="unresolved-note">
					These items couldn’t be priced, so they’re not included in any plan total.
					Open your list to clarify or edit these items.
				</p>
				<ul>
					{#each result.unresolved_items as item (item.item_index)}
						<li>
							<span>{item.display_text} — needs clarifying</span>
							<a class="clarify" href="/list/{listId}">Review on your list</a>
						</li>
					{/each}
					{#each result.no_valid_offer_items as item (item.item_index)}
						<li>
							<span>{item.display_text} — {reasonText(item.reason)}</span>
							<a
								class="clarify"
								href="/items/{item.canonical_item_id}/preferences?list={listId}"
								>{item.reason === 'no_matches' ? 'Find a match' : 'Set preferences'}</a
							>
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
	.trip {
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.85rem 1rem;
	}
	.trip h2 {
		margin: 0 0 0.25rem;
		font-size: 1rem;
	}
	.trip-note {
		margin: 0 0 0.6rem;
		font-size: 0.85rem;
		color: #64748b;
	}
	.trip-buttons {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.trip-store {
		padding: 0.4rem 0.85rem;
		border: 1px solid #cbd5e1;
		border-radius: 999px;
		background: #f8fafc;
		font-size: 0.9rem;
		cursor: pointer;
	}
	.trip-store.on {
		background: #0f766e;
		border-color: #0f766e;
		color: #fff;
	}
	.plans {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.explanation,
	.unresolved,
	.needs-confirmation,
	.aldi {
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 1rem;
	}
	/* The Aldi comparison (issue #100) is informational evidence, not a plan:
	   a cool accent keeps it visually separate from the routable plan cards. */
	.aldi {
		border-color: #bae6fd;
		background: #f0f9ff;
	}
	.explanation h2,
	.unresolved h2,
	.needs-confirmation h2,
	.aldi h2 {
		margin: 0 0 0.5rem;
		font-size: 1.05rem;
	}
	.explanation p {
		margin: 0;
		color: #334155;
		line-height: 1.5;
	}
	.aldi-summary {
		margin: 0 0 0.5rem;
		font-weight: 600;
		color: #0c4a6e;
	}
	.aldi-splits {
		list-style: none;
		margin: 0 0 0.5rem;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		font-size: 0.92rem;
		color: #334155;
	}
	.aldi-empty,
	.aldi-note {
		margin: 0;
		font-size: 0.85rem;
		color: #64748b;
	}
	.aldi-note {
		margin-top: 0.5rem;
	}
	.unresolved-note,
	.needs-confirmation-note {
		margin: 0 0 0.6rem;
		color: #64748b;
		font-size: 0.9rem;
	}
	.unresolved ul,
	.needs-confirmation ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.unresolved li,
	.needs-confirmation li {
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
	/* The ask-confirm chooser (issue #78): one block per item, each listing its
	   candidate offers with a Choose button. Overrides the row layout the
	   shared `.needs-confirmation li` rule sets for the old count-only view. */
	.confirm-list .confirm-item {
		display: flex;
		flex-direction: column;
		align-items: stretch;
		gap: 0.5rem;
	}
	.confirm-item-name {
		margin: 0;
		font-weight: 600;
		color: #1e293b;
	}
	.candidate-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}
	.confirm-list .candidate {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		padding: 0.5rem 0.6rem;
		background: #fff;
		border: 1px solid #fde68a;
		border-radius: 0.4rem;
	}
	.candidate-detail {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 0.5rem;
	}
	.candidate-name {
		font-weight: 600;
		color: #1e293b;
	}
	.candidate-meta {
		color: #64748b;
		font-size: 0.85rem;
	}
	.candidate-price {
		font-weight: 600;
		color: #0f766e;
	}
	.choose {
		flex-shrink: 0;
		padding: 0.35rem 0.85rem;
		border: 1px solid #0f766e;
		border-radius: 0.4rem;
		background: #0f766e;
		color: #fff;
		font-size: 0.88rem;
		cursor: pointer;
	}
	.choose:disabled {
		opacity: 0.6;
		cursor: progress;
	}
	.confirm-error {
		margin: 0;
		color: #b91c1c;
		font-size: 0.85rem;
	}
</style>
