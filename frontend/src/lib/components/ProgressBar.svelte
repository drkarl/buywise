<script lang="ts">
	// Shopping-mode progress bar: shows `bought / total` as a filled bar plus a
	// textual count. Presentational only — the page computes the counts off the
	// live Dexie entries and passes them in, so the bar updates the moment an
	// item is checked off.

	interface Props {
		bought: number;
		total: number;
	}

	let { bought, total }: Props = $props();

	// Guard against divide-by-zero for an empty shop; an empty list reads as 0%.
	const percent = $derived(total === 0 ? 0 : Math.round((bought / total) * 100));
	const complete = $derived(total > 0 && bought === total);
</script>

<div class="progress" class:complete>
	<div
		class="track"
		role="progressbar"
		aria-valuenow={bought}
		aria-valuemin={0}
		aria-valuemax={total}
		aria-label="Items bought"
	>
		<div class="fill" style:width={`${percent}%`}></div>
	</div>
	<span class="label" aria-live="polite">{bought} / {total} bought</span>
</div>

<style>
	.progress {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}
	.track {
		height: 0.6rem;
		background: #e2e8f0;
		border-radius: 0.3rem;
		overflow: hidden;
	}
	.fill {
		height: 100%;
		background: #0f766e;
		transition: width 0.2s ease-out;
	}
	.complete .fill {
		background: #16a34a;
	}
	.label {
		font-size: 0.85rem;
		color: #64748b;
	}
</style>
