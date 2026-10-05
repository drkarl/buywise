<script lang="ts">
	// Household store settings (issue #98). Lets the household tell BuyWise how it
	// actually shops so plans stop recommending stores it never visits:
//   * a status per store — active (always planned for), occasional (only when a
//     trip is marked on the optimise screen) or off (never planned for);
//   * a "nearby" flag, so a close store needs a smaller saving to be added to a
//     plan than the default second-store threshold;
//   * the two convenience thresholds (in euros in the UI, cents on the wire).
//
// Like the optimise/savings views this page is intrinsically online; SSR is
// disabled globally so the on-mount load only runs in the browser. All money is
// integer cents on the wire; the inputs convert to/from euros here.

import { retailerLabel } from '$lib/api/optimise';
import { fetchSettings, updateSettings } from '$lib/api/settings';
import type { Settings, StoreSetting, StoreStatus } from '$lib/api/settings';

type Status = 'loading' | 'ready' | 'error';

let status = $state<Status>('loading');
let stores = $state<StoreSetting[]>([]);
/** Convenience thresholds as euro strings for the number inputs ("10.00"). */
let threshold2Euros = $state('10.00');
let threshold3Euros = $state('25.00');
let saving = $state(false);
let savedMessage = $state('');
let saveError = $state('');

/** The status options, in the order they render, with their labels. */
const STATUS_OPTIONS: { value: StoreStatus; label: string }[] = [
	{ value: 'active', label: 'Active' },
	{ value: 'occasional', label: 'Occasional' },
	{ value: 'off', label: 'Off' }
];

/** Apply a server settings payload to the form state. */
function applySettings(settings: Settings) {
	stores = settings.stores;
	threshold2Euros = (settings.convenience_threshold_2_cents / 100).toFixed(2);
	threshold3Euros = (settings.convenience_threshold_3_cents / 100).toFixed(2);
}

async function load() {
	status = 'loading';
	savedMessage = '';
	saveError = '';
	try {
		applySettings(await fetchSettings());
		status = 'ready';
	} catch {
		status = 'error';
	}
}

$effect(() => {
	load();
});

function retry() {
	load();
}

/** Save the whole form: both thresholds and every store row. */
async function save(event: SubmitEvent) {
	event.preventDefault();
	savedMessage = '';
	saveError = '';

	const threshold2 = Math.round(Number(threshold2Euros) * 100);
	const threshold3 = Math.round(Number(threshold3Euros) * 100);
	if (
		!Number.isFinite(threshold2) ||
		!Number.isFinite(threshold3) ||
		threshold2 < 0 ||
		threshold3 < threshold2
	) {
		saveError =
			'Enter valid amounts: the second-store threshold must be at least €0 and no more than the third-store threshold.';
		return;
	}

	saving = true;
	try {
		const updated = await updateSettings({
			convenience_threshold_2_cents: threshold2,
			convenience_threshold_3_cents: threshold3,
			stores: stores.map((s) => ({
				retailer: s.retailer,
				status: s.status,
				nearby: s.nearby
			}))
		});
		applySettings(updated);
		savedMessage = 'Saved.';
	} catch {
		saveError = 'Couldn’t save your settings. Check your connection and try again.';
	} finally {
		saving = false;
	}
}
</script>

<svelte:head>
	<title>BuyWise — Store settings</title>
</svelte:head>

<main class="settings-view">
	<header class="settings-header">
		<h1>Store settings</h1>
		<a class="back" href="/">Home</a>
	</header>

	{#if status === 'loading'}
		<p class="loading" role="status" aria-live="polite">Loading your settings…</p>
	{:else if status === 'error'}
		<div class="error" role="alert">
			<p>Couldn’t load your settings. Check your connection and try again.</p>
			<button type="button" class="retry" onclick={retry}>Try again</button>
		</div>
	{:else}
		<form class="settings-form" onsubmit={save}>
			<section class="stores" aria-label="Stores">
				<h2>Your stores</h2>
				<p class="hint">
					Active stores are always considered. Occasional stores only count when you mark a
					trip to them on the optimise screen. Off stores are never planned for. Mark a store
					nearby when it is close enough that a short trip is worth a smaller saving.
				</p>
				<ul class="store-list">
					{#each stores as store (store.retailer)}
						<li class="store-row">
							<label class="store-name" for="status-{store.retailer}">
								{retailerLabel(store.retailer)}
							</label>
							<select
								id="status-{store.retailer}"
								class="status"
								aria-label="{retailerLabel(store.retailer)} status"
								bind:value={store.status}
							>
								{#each STATUS_OPTIONS as option (option.value)}
									<option value={option.value}>{option.label}</option>
								{/each}
							</select>
							<label class="nearby">
								<input
									type="checkbox"
									aria-label="{retailerLabel(store.retailer)} is nearby"
									bind:checked={store.nearby}
								/>
								<span>Nearby</span>
							</label>
						</li>
					{/each}
				</ul>
			</section>

			<section class="thresholds" aria-label="Convenience thresholds">
				<h2>Second-store threshold</h2>
				<p class="hint">
					How much a two-store split must save over the best single store before it is
					recommended. A nearby store clears a smaller bar (€5.00).
				</p>
				<div class="threshold-row">
					<label for="threshold-2">Second store saves at least (€)</label>
					<input
						id="threshold-2"
						type="number"
						min="0"
						step="0.01"
						inputmode="decimal"
						bind:value={threshold2Euros}
					/>
				</div>
				<div class="threshold-row">
					<label for="threshold-3">Third store saves at least (€)</label>
					<input
						id="threshold-3"
						type="number"
						min="0"
						step="0.01"
						inputmode="decimal"
						bind:value={threshold3Euros}
					/>
				</div>
			</section>

			{#if saveError}
				<p class="error" role="alert">{saveError}</p>
			{:else if savedMessage}
				<p class="saved" role="status">{savedMessage}</p>
			{/if}

			<button class="save" type="submit" disabled={saving}>
				{saving ? 'Saving…' : 'Save settings'}
			</button>
		</form>
	{/if}
</main>

<style>
	.settings-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}
	.settings-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.settings-header h1 {
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
		margin: 0;
		padding: 0.75rem 1rem;
		background: #fef2f2;
		border: 1px solid #fecaca;
		border-radius: 0.5rem;
		color: #991b1b;
	}
	.saved {
		margin: 0;
		padding: 0.6rem 1rem;
		background: #f0fdfa;
		border: 1px solid #99f6e4;
		border-radius: 0.5rem;
		color: #134e4a;
	}
	.settings-form {
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}
	h2 {
		font-size: 1.05rem;
		margin: 0 0 0.35rem;
	}
	.hint {
		margin: 0 0 0.75rem;
		font-size: 0.85rem;
		color: #64748b;
	}
	.store-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.store-row {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.6rem 0.75rem;
	}
	.store-name {
		flex: 1 1 auto;
		font-weight: 600;
	}
	.status {
		padding: 0.35rem 0.5rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
		background: #f8fafc;
	}
	.nearby {
		display: flex;
		align-items: center;
		gap: 0.35rem;
		font-size: 0.9rem;
	}
	.threshold-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		margin-bottom: 0.5rem;
	}
	.threshold-row label {
		font-size: 0.9rem;
	}
	.threshold-row input {
		width: 8rem;
		padding: 0.35rem 0.5rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
	}
	.save {
		align-self: flex-start;
		padding: 0.5rem 1.1rem;
		border: none;
		border-radius: 0.5rem;
		background: #0f766e;
		color: #fff;
		font-size: 0.95rem;
		cursor: pointer;
	}
	.save:disabled {
		opacity: 0.6;
		cursor: default;
	}
</style>
