<script lang="ts">
	// One priced line of a basket-optimiser plan, with accept/reject controls for
	// the underlying product match (P5-T1, issue #32). Presentational rendering of
	// the line (display text + freshness confidence, store, loyalty flag, price) is
	// kept here so `PlanCard` composes a list of these; the feedback buttons POST
	// the user's accept/reject decision to `POST /api/match-feedback`.
	//
	// A line always carries `retailer_product_id`; `canonical_item_id` is normally
	// present for a priced line but is typed nullable to match the wire contract —
	// when it is absent we cannot identify the match, so the feedback controls are
	// hidden rather than POSTing an incomplete pair.
	//
	// Feedback is fire-and-update-locally: we POST, then reflect 'accepted' /
	// 'rejected' inline. A failed POST surfaces a small retry-able error and leaves
	// the buttons enabled. This view does not re-fetch the plan; the next optimise
	// run reflects the persisted decision.

	import type { PlanLine } from '$lib/api/optimise';
	import {
		formatEuros,
		retailerLabel,
		sendMatchFeedback,
		type MatchFeedbackAction
	} from '$lib/api/optimise';
	import ConfidenceIndicator from './ConfidenceIndicator.svelte';

	interface Props {
		/** The plan line to render. */
		line: PlanLine;
	}

	let { line }: Props = $props();

	// Local feedback state for this line. `null` until the user acts; once a POST
	// succeeds we show the resulting status. `pending` disables the buttons during
	// the in-flight request to avoid a double-submit.
	let decision = $state<'accepted' | 'rejected' | null>(null);
	let pending = $state(false);
	let errorMessage = $state('');

	// We can only give feedback when the line identifies BOTH sides of the match.
	const canGiveFeedback = $derived(line.canonical_item_id !== null);

	async function submit(action: MatchFeedbackAction) {
		if (!line.canonical_item_id || pending) return;
		pending = true;
		errorMessage = '';
		try {
			const { ok } = await sendMatchFeedback({
				canonicalItemId: line.canonical_item_id,
				retailerProductId: line.retailer_product_id,
				action
			});
			if (ok) {
				// `prefer` is not surfaced as a button here (accept/reject only, per
				// the issue), so the decision label is the action directly.
				decision = action === 'reject' ? 'rejected' : 'accepted';
			} else {
				errorMessage = 'Couldn’t save — try again.';
			}
		} catch {
			errorMessage = 'Couldn’t reach the server — try again.';
		} finally {
			pending = false;
		}
	}
</script>

<li class="line">
	<span class="line-name">
		{line.display_text}
		<ConfidenceIndicator confidence={line.freshness_confidence} />
	</span>
	<span class="line-meta">
		<span class="line-store">{retailerLabel(line.retailer)}</span>
		{#if line.loyalty_applied}
			<span class="loyalty" title="Loyalty price applied">loyalty</span>
		{/if}
		<span class="line-price">{formatEuros(line.line_total_cents)}</span>
	</span>

	{#if canGiveFeedback}
		<span class="feedback">
			{#if decision === 'accepted'}
				<span class="decided accepted" role="status">Accepted</span>
			{:else if decision === 'rejected'}
				<span class="decided rejected" role="status">Rejected</span>
			{:else}
				<button
					type="button"
					class="accept"
					disabled={pending}
					aria-label={`Accept match for ${line.display_text}`}
					onclick={() => submit('accept')}
				>
					Accept
				</button>
				<button
					type="button"
					class="reject"
					disabled={pending}
					aria-label={`Reject match for ${line.display_text}`}
					onclick={() => submit('reject')}
				>
					Reject
				</button>
			{/if}
		</span>
	{/if}

	{#if errorMessage}
		<span class="feedback-error" role="alert">{errorMessage}</span>
	{/if}
</li>

<style>
	.line {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		flex-wrap: wrap;
		font-size: 0.92rem;
	}
	.line-name {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
	}
	.line-meta {
		display: inline-flex;
		align-items: center;
		gap: 0.5rem;
		color: #64748b;
		flex-shrink: 0;
	}
	.line-store {
		font-size: 0.8rem;
	}
	.loyalty {
		font-size: 0.7rem;
		text-transform: uppercase;
		letter-spacing: 0.03em;
		color: #0f766e;
		background: #ccfbf1;
		border-radius: 0.3rem;
		padding: 0.05rem 0.35rem;
	}
	.line-price {
		color: #0f172a;
		font-variant-numeric: tabular-nums;
	}
	.feedback {
		display: inline-flex;
		align-items: center;
		gap: 0.4rem;
		flex-shrink: 0;
	}
	.feedback button {
		font-size: 0.78rem;
		padding: 0.2rem 0.55rem;
		border-radius: 0.4rem;
		border: 1px solid #cbd5e1;
		background: #fff;
		cursor: pointer;
	}
	.feedback button:disabled {
		opacity: 0.55;
		cursor: default;
	}
	.feedback .accept {
		color: #047857;
		border-color: #6ee7b7;
	}
	.feedback .reject {
		color: #b91c1c;
		border-color: #fca5a5;
	}
	.decided {
		font-size: 0.78rem;
		font-weight: 600;
	}
	.decided.accepted {
		color: #047857;
	}
	.decided.rejected {
		color: #b91c1c;
	}
	.feedback-error {
		flex-basis: 100%;
		font-size: 0.78rem;
		color: #b91c1c;
	}
</style>
