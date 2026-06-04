<script lang="ts">
	// A single collapsible category section in shopping mode. Header shows the
	// category name and the count of items still to buy ("remaining"). Items stay
	// in place when checked off — bought items render struck-through within the
	// same group rather than moving to a separate section, so the shopper sees
	// what's done in context (P2-T6 behaviour).

	import type { CategoryGroup } from '$lib/shopping/sortByStoreOrder';

	interface Props {
		group: CategoryGroup;
		/** Check an item off (active → bought). */
		onbought: (id: string) => void;
	}

	let { group, onbought }: Props = $props();

	// Sections default to open. We do not auto-collapse a fully-bought category:
	// keeping it open preserves the "see what's done in context" behaviour.
	let open = $state(true);

	const title = $derived(group.category?.name_en ?? 'Other');
</script>

<section class="category">
	<button
		type="button"
		class="header"
		aria-expanded={open}
		onclick={() => (open = !open)}
	>
		<span class="caret" aria-hidden="true">{open ? '▾' : '▸'}</span>
		<span class="title">{title}</span>
		<span class="remaining">{group.remaining} left</span>
	</button>

	{#if open}
		<ul class="items">
			{#each group.items as item (item.id)}
				{@const isBought = item.status === 'bought'}
				<li class="item" class:bought={isBought}>
					{#if isBought}
						<!-- Bought items stay in place, struck through. Un-checking is not
						     a canonical mutation (see crates/api/src/mutations.rs eight
						     operation types), so the marker is static, matching ListItem. -->
						<span class="check done" aria-hidden="true">●</span>
					{:else}
						<button
							type="button"
							class="check"
							aria-label="Check off {item.display_name}"
							onclick={() => onbought(item.id)}
						>
							○
						</button>
					{/if}
					<span class="name">{item.display_name}</span>
					{#if item.quantity > 1}
						<span class="qty">×{item.quantity}</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>

<style>
	.category {
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		overflow: hidden;
	}
	.header {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		width: 100%;
		padding: 0.6rem 0.75rem;
		background: #f1f5f9;
		border: none;
		font-weight: 600;
		cursor: pointer;
		text-align: left;
	}
	.title {
		flex: 1;
	}
	.remaining {
		font-size: 0.8rem;
		font-weight: 400;
		color: #64748b;
	}
	.items {
		margin: 0;
		padding: 0;
		list-style: none;
	}
	.item {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.6rem 0.75rem;
		border-top: 1px solid #e2e8f0;
		background: #fff;
	}
	.item.bought .name {
		text-decoration: line-through;
		color: #94a3b8;
	}
	.check {
		width: 1.75rem;
		height: 1.75rem;
		font-size: 1.1rem;
		line-height: 1;
		border: none;
		background: transparent;
		cursor: pointer;
		color: #0f766e;
	}
	.name {
		flex: 1;
		font-size: 1rem;
	}
	.qty {
		min-width: 1.25rem;
		text-align: center;
		font-size: 0.9rem;
		color: #64748b;
	}
</style>
