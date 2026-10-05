<script lang="ts">
	// The "Suggested" section of the list view (issue #106).
	//
	// Presentational: it renders the computed suggestions and forwards confirm
	// (add to the list) and dismiss (push the next suggestion later) to the
	// parent, which owns the suggestion store and the offline mutation queue.
	// Deliberately separate from the active list so a suggestion is never
	// mistaken for an item already on the list — nothing is added without the
	// user confirming it.

	import type { Suggestion } from '$lib/stores/suggestions';

	interface Props {
		suggestions: Suggestion[];
		onconfirm: (suggestion: Suggestion) => void;
		ondismiss: (suggestion: Suggestion) => void;
	}

	let { suggestions, onconfirm, ondismiss }: Props = $props();
</script>

{#if suggestions.length > 0}
	<section class="suggested" aria-label="Suggested">
		<h2 class="heading">Suggested</h2>
		<ul class="items">
			{#each suggestions as suggestion (suggestion.canonical_item_id)}
				<li class="row">
					<div class="info">
						<span class="name">{suggestion.display_name}</span>
						<span class="hint"
							>usually every {suggestion.typical_interval_days}
							{suggestion.typical_interval_days === 1 ? 'day' : 'days'} · last bought
							{suggestion.days_since_last_purchase}
							{suggestion.days_since_last_purchase === 1 ? 'day' : 'days'} ago</span
						>
					</div>
					<div class="actions">
						<button
							type="button"
							class="add"
							onclick={() => onconfirm(suggestion)}
							aria-label={`Add ${suggestion.display_name}`}>Add</button
						>
						<button
							type="button"
							class="dismiss"
							onclick={() => ondismiss(suggestion)}
							aria-label={`Dismiss ${suggestion.display_name}`}>Dismiss</button
						>
					</div>
				</li>
			{/each}
		</ul>
	</section>
{/if}

<style>
	.suggested {
		border: 1px solid #bae6fd;
		background: #f0f9ff;
		border-radius: 0.5rem;
		padding: 0.75rem;
	}
	.heading {
		margin: 0 0 0.5rem;
		font-size: 0.95rem;
		color: #075985;
	}
	.items {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
	}
	.info {
		display: flex;
		flex-direction: column;
	}
	.name {
		font-weight: 600;
	}
	.hint {
		font-size: 0.8rem;
		color: #64748b;
	}
	.actions {
		display: flex;
		gap: 0.4rem;
	}
	.add,
	.dismiss {
		font-size: 0.85rem;
		border-radius: 0.4rem;
		padding: 0.3rem 0.6rem;
		cursor: pointer;
	}
	.add {
		font-weight: 600;
		color: #ffffff;
		background: #0369a1;
		border: 1px solid #0369a1;
	}
	.dismiss {
		color: #075985;
		background: transparent;
		border: 1px solid #7dd3fc;
	}
</style>
