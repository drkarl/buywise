<script lang="ts">
	// Inline duplicate warning shown before an add is committed. Phase 2 uses
	// dictionary-only client-side detection (no server call), so this region just
	// surfaces the already-computed match and offers three choices:
	//   • Add anyway (+1): commits a fresh-UUID `add_item` via the normal add path.
	//     Both rows stay separate (each `unresolved`, distinct
	//     `effective_constraint_hash = 'unresolved:<entry_id>'`) until P3-T1's
	//     conflict handler merges them once they resolve to the same canonical item.
	//   • Merge quantity: DISABLED in Phase 2. There is no sync-safe quantity-merge
	//     op — `apply_add_item` treats a reused entity_id as a PK collision (409),
	//     and the dedup index only fires for resolved/active rows. Tracked in #60.
	//     Rendered disabled with an associated explanatory note (not just `title`).
	//   • Cancel: dismisses the warning, leaving the typed text in the caller's input.

	import type { DuplicateMatch } from '$lib/matching/duplicate';

	interface Props {
		/** The raw text the user is trying to add (display form). */
		newItemText: string;
		/** The duplicate match detected client-side. */
		match: DuplicateMatch;
		/** Commit the add as a separate fresh-UUID entry. */
		onaddanyway: () => void;
		/** Dismiss the warning without adding. */
		oncancel: () => void;
	}

	let { newItemText, match, onaddanyway, oncancel }: Props = $props();

	const existingName = $derived(match.matchedItem.display_name);
</script>

<div class="duplicate-warning" role="alert" aria-live="assertive">
	<p class="message">
		<strong>“{existingName}”</strong> is already on your list.
	</p>

	<p class="add-anyway-note" id="duplicate-add-anyway-note">
		“Add anyway” keeps “{newItemText}” as a separate entry. The two stay separate
		until they resolve to the same item, when they’ll be merged automatically.
	</p>

	<div class="actions">
		<button
			type="button"
			class="primary"
			aria-describedby="duplicate-add-anyway-note"
			onclick={onaddanyway}
		>
			Add anyway (+1)
		</button>

		<button
			type="button"
			class="merge"
			disabled
			aria-disabled="true"
			title="Requires syncable quantity mutation, tracked in #60."
			aria-describedby="duplicate-merge-note"
		>
			Merge quantity
		</button>

		<button type="button" class="cancel" onclick={oncancel}> Cancel </button>
	</div>

	<p class="merge-note" id="duplicate-merge-note">
		Merge quantity isn’t available yet — it needs a syncable quantity mutation
		(tracked in #60).
	</p>
</div>

<style>
	.duplicate-warning {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.75rem;
		background: #fffbeb;
		border: 1px solid #fde68a;
		border-radius: 0.5rem;
		color: #92400e;
		font-size: 0.9rem;
	}
	.message {
		margin: 0;
	}
	.add-anyway-note,
	.merge-note {
		margin: 0;
		font-size: 0.8rem;
		color: #b45309;
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.actions button {
		padding: 0.45rem 0.75rem;
		font-size: 0.9rem;
		font-weight: 600;
		border-radius: 0.4rem;
		border: 1px solid transparent;
		cursor: pointer;
	}
	.primary {
		color: #fff;
		background: #0f766e;
	}
	.merge {
		color: #475569;
		background: #f1f5f9;
		border-color: #cbd5e1;
	}
	.merge:disabled {
		opacity: 0.55;
		cursor: not-allowed;
	}
	.cancel {
		color: #92400e;
		background: transparent;
		border-color: #fcd34d;
	}
</style>
