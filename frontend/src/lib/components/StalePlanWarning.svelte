<script lang="ts">
	// Stale-plan warning for a cached chosen trip (issue #104 follow-up).
	//
	// Shared by the trip page and the store-scoped shopping mode so both surface
	// the same message and the same review path back to the optimiser instead of
	// copying the wording. `isTripStale` (chosenTrip.ts) decides when to show it.
	let { listId }: { listId: string } = $props();
</script>

<p class="stale" role="alert">
	Your list has changed since you chose this plan, so it may not cover everything below.
	<a class="link" href="/list/{listId}/optimise">Review and choose a plan again</a>.
</p>

<style>
	.link {
		font-size: 0.9rem;
		color: #0f766e;
	}
	.stale {
		margin: 0;
		padding: 0.75rem;
		background: #fffbeb;
		border: 1px solid #fde68a;
		border-radius: 0.5rem;
		color: #92400e;
		font-size: 0.9rem;
	}
</style>
