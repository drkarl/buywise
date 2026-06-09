<script lang="ts">
	// A single list row. Renders an entry and surfaces its operations via callback
	// props; all Dexie/mutation logic lives in the parent + store, keeping this
	// component presentational. Delete is offered two ways: swipe-to-delete (touch
	// drag past a threshold) and a long-press context menu (also keyboard/mouse
	// accessible via the explicit Delete button it reveals).

	import type { LocalListEntry } from '$lib/db/dexie';

	interface Props {
		entry: LocalListEntry;
		onbought: (id: string) => void;
		onskipped: (id: string) => void;
		ondelete: (id: string) => void;
		/**
		 * Open the disambiguation flow for an `ambiguous` entry (P3-T3). Optional so
		 * existing call sites that don't surface clarification still type-check; when
		 * absent the "?" indicator is not shown.
		 */
		onclarify?: (id: string) => void;
		/**
		 * Set the entry's quantity to an absolute value (issue #60). Optional so call
		 * sites that don't allow quantity edits (e.g. the read-only basket section)
		 * still type-check; when absent the +/- stepper is not shown and the static
		 * `×N` is rendered instead.
		 */
		onquantity?: (id: string, quantity: number) => void;
	}

	let { entry, onbought, onskipped, ondelete, onclarify, onquantity }: Props = $props();

	const inBasket = $derived(entry.status === 'bought' || entry.status === 'skipped');
	// An active, ambiguous entry the parent can disambiguate.
	const isAmbiguous = $derived(
		entry.resolution_status === 'ambiguous' && !inBasket && onclarify !== undefined
	);
	// Show the +/- stepper only for an active (not in-basket) entry when the parent
	// wired `onquantity`. The `-` button is disabled at quantity 1 so it never goes
	// below 1, and `+` is disabled at MAX_QUANTITY so the UI never round-trips a
	// value the server will reject (mirrors the backend's `1..=9999` rule).
	const showStepper = $derived(!inBasket && onquantity !== undefined);
	// Upper bound for the quantity stepper; mirrors the backend's MAX_QUANTITY
	// (crates/api/src/mutations.rs). Keep the two in sync.
	const MAX_QUANTITY = 9999;

	// Swipe-to-delete state.
	const SWIPE_THRESHOLD = 80;
	const LONG_PRESS_MS = 500;
	let dragX = $state(0);
	let startX = 0;
	let dragging = false;
	let menuOpen = $state(false);
	let longPressTimer: ReturnType<typeof setTimeout> | undefined;

	function onpointerdown(event: PointerEvent) {
		startX = event.clientX;
		dragging = true;
		// Capture the pointer so we keep getting move/up events even if the pointer
		// leaves the element mid-swipe (otherwise a fast drag can drop the gesture).
		// Guarded: not all environments (e.g. jsdom) implement pointer capture.
		const target = event.currentTarget as HTMLElement;
		target.setPointerCapture?.(event.pointerId);
		longPressTimer = setTimeout(() => {
			menuOpen = true;
			dragging = false;
		}, LONG_PRESS_MS);
	}

	function onpointermove(event: PointerEvent) {
		if (!dragging) return;
		const delta = event.clientX - startX;
		if (Math.abs(delta) > 6) clearTimeout(longPressTimer);
		dragX = Math.min(0, delta); // only allow left swipe
	}

	function onpointerup() {
		clearTimeout(longPressTimer);
		dragging = false;
		if (dragX <= -SWIPE_THRESHOLD) {
			ondelete(entry.id);
		}
		dragX = 0;
	}
</script>

<li class="row" class:in-basket={inBasket}>
	<!-- The pointer handlers implement swipe-to-delete / long-press; the actual
	     actions are also reachable via the explicit buttons inside, so this layer
	     is a presentational gesture surface. -->
	<div
		class="swipe"
		role="group"
		aria-label={entry.display_name}
		style:transform="translateX({dragX}px)"
		{onpointerdown}
		{onpointermove}
		{onpointerup}
		onpointercancel={onpointerup}
	>
		{#if !inBasket}
			<button
				class="check"
				aria-label="Check off {entry.display_name}"
				onclick={() => onbought(entry.id)}>○</button
			>
		{:else}
			<span class="check done" aria-hidden="true">●</span>
		{/if}

		<span class="name">{entry.display_name}</span>

		{#if isAmbiguous}
			<button
				type="button"
				class="clarify"
				aria-label={`Clarify ${entry.display_name}`}
				onclick={() => onclarify?.(entry.id)}>? Tap to clarify</button
			>
		{/if}

		{#if showStepper}
			<div class="qty" role="group" aria-label="Quantity for {entry.display_name}">
				<button
					type="button"
					class="qty-btn"
					aria-label="Decrease quantity of {entry.display_name}"
					disabled={entry.quantity <= 1}
					onclick={() => onquantity?.(entry.id, entry.quantity - 1)}>−</button
				>
				<span class="qty-value" aria-live="polite">{entry.quantity}</span>
				<button
					type="button"
					class="qty-btn"
					aria-label="Increase quantity of {entry.display_name}"
					disabled={entry.quantity >= MAX_QUANTITY}
					onclick={() => onquantity?.(entry.id, entry.quantity + 1)}>+</button
				>
			</div>
		{:else if entry.quantity > 1}
			<span class="qty-value">×{entry.quantity}</span>
		{/if}

		{#if !inBasket}
			<button class="skip" aria-label="Skip {entry.display_name}" onclick={() => onskipped(entry.id)}
				>Skip</button
			>
		{:else}
			<span class="status-tag">{entry.status === 'bought' ? 'Bought' : 'Skipped'} ×{entry.quantity}</span>
		{/if}
	</div>

	{#if menuOpen}
		<div class="context-menu" role="group" aria-label="Item actions">
			<button onclick={() => { menuOpen = false; ondelete(entry.id); }}>Delete</button>
			<button onclick={() => (menuOpen = false)}>Cancel</button>
		</div>
	{/if}
</li>

<style>
	.row {
		position: relative;
		list-style: none;
		border-bottom: 1px solid #e2e8f0;
		background: #fee2e2; /* revealed under the swipe layer */
	}
	.swipe {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		padding: 0.65rem 0.75rem;
		background: #fff;
		touch-action: pan-y;
		transition: transform 0.12s ease-out;
	}
	.in-basket .swipe {
		opacity: 0.6;
	}
	.name {
		flex: 1;
		font-size: 1rem;
	}
	.in-basket .name {
		text-decoration: line-through;
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
	.check.done {
		color: #0f766e;
	}
	.qty {
		display: flex;
		align-items: center;
		gap: 0.25rem;
	}
	.qty-btn {
		width: 1.6rem;
		height: 1.6rem;
		font-size: 1.1rem;
		line-height: 1;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
		background: #f8fafc;
		color: #0f172a;
		cursor: pointer;
	}
	.qty-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	.qty-value {
		min-width: 1.25rem;
		text-align: center;
		font-size: 0.9rem;
		color: #64748b;
	}
	.skip {
		padding: 0.3rem 0.6rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
		background: #f8fafc;
		cursor: pointer;
	}
	.clarify {
		padding: 0.25rem 0.55rem;
		border: 1px solid #f59e0b;
		border-radius: 0.4rem;
		background: #fffbeb;
		color: #b45309;
		font-size: 0.8rem;
		cursor: pointer;
		white-space: nowrap;
	}
	.status-tag {
		font-size: 0.85rem;
		color: #64748b;
	}
	.context-menu {
		display: flex;
		gap: 0.5rem;
		padding: 0.5rem 0.75rem;
		background: #f1f5f9;
		border-top: 1px solid #e2e8f0;
	}
	.context-menu button {
		padding: 0.3rem 0.75rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
		background: #fff;
		cursor: pointer;
	}
</style>
