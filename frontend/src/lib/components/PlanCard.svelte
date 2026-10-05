<script lang="ts">
	// A single basket-optimiser plan card (P4-T3). Presentational: it renders one
	// `StorePlan` — the store(s) to visit, the priced lines (each with its
	// freshness-confidence indicator), the total in euros, the saving vs the best
	// single-store plan, and the Dunnes voucher text inline when the plan carries
	// one. All money arrives as integer cents and is formatted here.
	//
	// The card has no behaviour of its own; the parent route decides which plans
	// exist and which one is `recommended` (visually highlighted via the flag).
	// The theoretical-minimum benchmark passes `benchmark` so we can label it as a
	// reference figure that is never actually routed.

	import type { StorePlan } from '$lib/api/optimise';
	import { formatEuros, retailerLabel } from '$lib/api/optimise';
	import PlanItem from './PlanItem.svelte';

	interface Props {
		/** The plan to display. */
		plan: StorePlan;
		/** Short kind label, e.g. "Best single store" / "Cheapest split". */
		title: string;
		/** When true, this is the optimiser's pick — render it visually distinct. */
		recommended?: boolean;
		/**
		 * Total cents of the best single-store plan, for the "saving vs best single"
		 * line. Null when there is no single-store baseline (then we show no saving).
		 */
		bestSingleTotalCents?: number | null;
		/** Item indices priced by the best single-store plan. */
		bestSingleItemIndices?: number[];
		/**
		 * True for the theoretical-minimum card: a costing benchmark that is never
		 * routed (may split across many stores). Labelled as a reference, not a plan
		 * to follow, and its saving line is suppressed.
		 */
		benchmark?: boolean;
		/**
		 * Plain-language reasons for this plan (issue #103), ordered most important
		 * first. At most four are shown; absent/empty renders nothing extra.
		 */
		reasons?: string[];
		/**
		 * When set, the card shows a "Shop this plan" link to this href. The parent
		 * route builds the chosen-trip allocation in `onchoose` before navigation
		 * (issue #104); the benchmark card passes none (it is not routable).
		 */
		chooseHref?: string | null;
		/** Persists the chosen trip; run before the `chooseHref` navigation. */
		onchoose?: (() => void) | null;
	}

	let {
		plan,
		title,
		recommended = false,
		bestSingleTotalCents = null,
		bestSingleItemIndices = [],
		benchmark = false,
		reasons = [],
		chooseHref = null,
		onchoose = null
	}: Props = $props();

	// Saving vs the best single-store plan = bestSingle.total − thisPlan.total.
	// Only meaningful when a single-store baseline exists and this isn't that very
	// baseline or the never-routed benchmark. May be <= 0 (a split can cost more).
	const savingCents = $derived.by(() => {
		if (bestSingleTotalCents === null || benchmark) return null;
		const baselineItems = [...bestSingleItemIndices].sort((a, b) => a - b);
		const planItems = plan.lines.map((line) => line.item_index).sort((a, b) => a - b);
		if (
			baselineItems.length !== planItems.length ||
			baselineItems.some((item, index) => item !== planItems[index])
		) {
			return null;
		}
		return bestSingleTotalCents - plan.total_cents;
	});

	const storeNames = $derived(plan.stores.map(retailerLabel).join(' + '));

	// The server prioritizes reasons and sends at most four for this plan.
	const shownReasons = $derived(reasons.slice(0, 4));
</script>

<article
	class="plan-card"
	class:recommended
	aria-label={recommended ? `${title} (recommended)` : title}
>
	<header class="plan-header">
		<div class="title-row">
			<h3 class="title">{title}</h3>
			{#if recommended}
				<span class="recommended-badge">Recommended</span>
			{/if}
		</div>
		<p class="stores">{storeNames}</p>
	</header>

	<p class="total" aria-label="Total {formatEuros(plan.total_cents)}">
		{formatEuros(plan.total_cents)}
	</p>

	{#if benchmark}
		<p class="benchmark-note">Costing benchmark — not a route to follow.</p>
	{:else if savingCents !== null && savingCents > 0}
		<p class="saving saving-positive">Saves {formatEuros(savingCents)} vs best single store</p>
	{:else if savingCents !== null && savingCents < 0}
		<p class="saving saving-negative">
			{formatEuros(-savingCents)} more than best single store
		</p>
	{:else if savingCents !== null}
		<p class="saving">Same price as best single store</p>
	{/if}

	{#if plan.dunnes_voucher_offer_text}
		<p class="voucher">🎟️ Dunnes voucher: {plan.dunnes_voucher_offer_text}</p>
	{/if}

	{#if shownReasons.length > 0}
		<ul class="reasons" aria-label="Why this plan">
			{#each shownReasons as reason, index (index)}
				<li>{reason}</li>
			{/each}
		</ul>
	{/if}

	<ul class="lines">
		{#each plan.lines as line (line.item_index)}
			<PlanItem {line} />
		{/each}
	</ul>

	{#if plan.missing_count > 0}
		<p class="missing">
			{plan.missing_count} item{plan.missing_count === 1 ? '' : 's'} {plan.missing_count === 1
				? 'has'
				: 'have'} no known price here and {plan.missing_count === 1 ? 'is' : 'are'} excluded from
			the total
		</p>
	{/if}

	{#if chooseHref}
		<a class="choose" href={chooseHref} onclick={() => onchoose?.()}>Shop this plan</a>
	{/if}
</article>

<style>
	.plan-card {
		border: 1px solid #e2e8f0;
		border-radius: 0.75rem;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		background: #fff;
	}
	.plan-card.recommended {
		border-color: #0f766e;
		box-shadow: 0 0 0 2px rgba(15, 118, 110, 0.18);
		background: #f0fdfa;
	}
	.plan-header {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.title-row {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}
	.title {
		margin: 0;
		font-size: 1.05rem;
	}
	.recommended-badge {
		font-size: 0.7rem;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: #0f766e;
		background: #ccfbf1;
		border-radius: 999px;
		padding: 0.1rem 0.5rem;
	}
	.stores {
		margin: 0;
		color: #475569;
		font-size: 0.9rem;
	}
	.total {
		margin: 0;
		font-size: 1.6rem;
		font-weight: 700;
	}
	.benchmark-note {
		margin: 0;
		font-size: 0.85rem;
		color: #64748b;
		font-style: italic;
	}
	.saving {
		margin: 0;
		font-size: 0.9rem;
		color: #475569;
	}
	.saving-positive {
		color: #047857;
		font-weight: 600;
	}
	.saving-negative {
		color: #b45309;
	}
	.voucher {
		margin: 0;
		font-size: 0.9rem;
		color: #92400e;
		background: #fffbeb;
		border: 1px solid #fde68a;
		border-radius: 0.5rem;
		padding: 0.4rem 0.6rem;
	}
	.reasons {
		margin: 0;
		padding-left: 1.1rem;
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		font-size: 0.85rem;
		line-height: 1.4;
		color: #475569;
	}
	.lines {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		border-top: 1px solid #f1f5f9;
		padding-top: 0.6rem;
	}
	.missing {
		margin: 0;
		font-size: 0.85rem;
		color: #94a3b8;
	}
	.choose {
		align-self: flex-start;
		margin-top: 0.1rem;
		padding: 0.4rem 0.9rem;
		border-radius: 0.5rem;
		background: #0f766e;
		color: #fff;
		font-size: 0.9rem;
		font-weight: 600;
		text-decoration: none;
	}
</style>
