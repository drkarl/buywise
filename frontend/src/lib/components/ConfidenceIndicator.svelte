<script lang="ts">
	// Per-line freshness confidence indicator for optimiser plan lines (P4-T3).
	// Presentational and intentionally quiet: it renders a VISIBLE badge ONLY for
	// `low` and `unknown` confidence — the cases a shopper should second-guess.
	// For `high` confidence it renders nothing at all (no visual noise, no marker),
	// which is the common case for a fresh offer.
	//
	// The badge carries a `title` + accessible `aria-label` so the meaning is
	// reachable by hover and by assistive tech, and exposes role="status" so tests
	// (and screen readers) can find it by its accessible name.

	import type { FreshnessConfidence } from '$lib/api/optimise';

	interface Props {
		/** The line's freshness confidence; only `low`/`unknown` render a badge. */
		confidence: FreshnessConfidence;
	}

	let { confidence }: Props = $props();

	// Copy per non-high level. `high` is absent on purpose: it produces no badge.
	const LABELS: Record<Exclude<FreshnessConfidence, 'high'>, string> = {
		low: 'Low price-freshness confidence',
		unknown: 'Unknown price freshness'
	};

	const show = $derived(confidence === 'low' || confidence === 'unknown');
	const label = $derived(show ? LABELS[confidence as 'low' | 'unknown'] : '');
</script>

{#if show}
	<span
		class="confidence {confidence}"
		role="status"
		aria-label={label}
		title={label}
	>
		<span aria-hidden="true">!</span>
		<span class="sr-only">{label}</span>
	</span>
{/if}

<style>
	.confidence {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.1rem;
		height: 1.1rem;
		border-radius: 999px;
		font-size: 0.75rem;
		font-weight: 700;
		line-height: 1;
		flex-shrink: 0;
	}
	/* `unknown` is the louder warning (no data at all); `low` is a soft caution. */
	.confidence.unknown {
		background: #fef2f2;
		color: #b91c1c;
		border: 1px solid #fecaca;
	}
	.confidence.low {
		background: #fffbeb;
		color: #b45309;
		border: 1px solid #fde68a;
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
		border: 0;
	}
</style>
