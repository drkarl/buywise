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
import {
	deleteHousehold,
	downloadHouseholdExport,
	fetchHousehold,
	HttpResponseError,
	redirectToSignIn,
	type Household
} from '$lib/api/household';
import { fetchSettings, updateSettings } from '$lib/api/settings';
import type {
	Category,
	CategoryRule,
	Settings,
	StoreSetting,
	StoreStatus,
	Subcategory
} from '$lib/api/settings';
import { isAllowed, withScope } from '$lib/settings/categoryRules';

type Status = 'loading' | 'ready' | 'error';

let status = $state<Status>('loading');
let stores = $state<StoreSetting[]>([]);
/** Shared category catalogue (ordered) used to render the per-store rules. */
let categories = $state<Category[]>([]);
/** Shared sub-category catalogue, grouped by parent for the nested checkboxes. */
let subcategories = $state<Subcategory[]>([]);
/** Each store's current category rules, keyed by retailer. */
let rulesByStore = $state<Record<string, CategoryRule[]>>({});
/** Convenience thresholds as euro strings for the number inputs ("10.00"). */
let threshold2Euros = $state('10.00');
let threshold3Euros = $state('25.00');
let saving = $state(false);
let savedMessage = $state('');
let saveError = $state('');

// Household data controls (issue #136): the household's id/name, the guarded
// delete form, and the export download. Loaded separately from the settings so
// a failure in one does not blank the other.
let householdStatus = $state<Status>('loading');
let household = $state<Household | null>(null);
let confirmName = $state('');
let confirmPassword = $state('');
let deleting = $state(false);
let deleteError = $state('');
let exporting = $state(false);
let exportError = $state('');

/** The delete runs only once the exact household name and a password are given. */
let canDelete = $derived(
	household !== null && confirmName === household.name && confirmPassword.length > 0
);

/** The status options, in the order they render, with their labels. */
const STATUS_OPTIONS: { value: StoreStatus; label: string }[] = [
	{ value: 'active', label: 'Active' },
	{ value: 'occasional', label: 'Occasional' },
	{ value: 'off', label: 'Off' }
];

/** Sub-categories of a category, in catalogue (display) order. */
function subcategoriesOf(categoryId: string): Subcategory[] {
	return subcategories.filter((s) => s.category_id === categoryId);
}

/** Apply a server settings payload to the form state. */
function applySettings(settings: Settings) {
	stores = settings.stores;
	categories = settings.categories;
	subcategories = settings.subcategories;
	rulesByStore = Object.fromEntries(
		settings.stores.map((s) => [s.retailer, s.category_rules])
	);
	threshold2Euros = (settings.convenience_threshold_2_cents / 100).toFixed(2);
	threshold3Euros = (settings.convenience_threshold_3_cents / 100).toFixed(2);
}

/** The rules currently configured for a store (empty = all categories allowed). */
function rulesFor(retailer: string): CategoryRule[] {
	return rulesByStore[retailer] ?? [];
}

/** Whether the given scope is allowed for a store. */
function allowedFor(retailer: string, categoryId: string, subcategoryId: string | null = null) {
	return isAllowed(rulesFor(retailer), categoryId, subcategoryId);
}

/** Flip one scope's checkbox, normalising the store's rule list. */
function setAllowed(
	retailer: string,
	categoryId: string,
	subcategoryId: string | null,
	allowed: boolean
) {
	rulesByStore = {
		...rulesByStore,
		[retailer]: withScope(rulesFor(retailer), categoryId, subcategoryId, allowed)
	};
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
	loadHousehold();
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
				nearby: s.nearby,
				category_rules: rulesFor(s.retailer)
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

/** Load the caller's household (id + name) for the data-controls section. */
async function loadHousehold() {
	householdStatus = 'loading';
	try {
		household = await fetchHousehold();
		householdStatus = 'ready';
	} catch {
		householdStatus = 'error';
	}
}

/** Download the household export as a file. */
async function downloadExport() {
	if (!household) return;
	exportError = '';
	exporting = true;
	try {
		await downloadHouseholdExport(household.id);
	} catch (err) {
		// A 401 means the session has gone: send the member back to sign in.
		if (err instanceof HttpResponseError && err.status === 401) {
			redirectToSignIn();
			return;
		}
		exportError = 'Couldn’t download your export. Please try again.';
	} finally {
		exporting = false;
	}
}

/** Delete the household after the typed-name + password guard, then sign out. */
async function removeHousehold(event: SubmitEvent) {
	event.preventDefault();
	if (!household || !canDelete) return;
	deleteError = '';
	deleting = true;
	try {
		await deleteHousehold(household.id, confirmName, confirmPassword);
		// The server ended the session; leave for the sign-in page.
		redirectToSignIn();
	} catch (err) {
		confirmPassword = '';
		if (err instanceof HttpResponseError && err.status === 401) {
			redirectToSignIn();
			return;
		}
		deleteError =
			err instanceof HttpResponseError && err.status === 422
				? 'The household name or your password didn’t match. Nothing was deleted.'
				: 'Couldn’t delete your household data. Please try again.';
	} finally {
		deleting = false;
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
							<div class="store-controls">
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
							</div>
							<details class="store-rules">
								<summary>Allowed categories</summary>
								<p class="hint rules-hint">
									Untick a category to keep this store out of plans for it. Untick a
									sub-category to refine a broad category.
								</p>
								<ul class="category-list">
									{#each categories as category (category.id)}
										<li class="category-item">
											<label class="rule">
												<input
													type="checkbox"
													aria-label="{retailerLabel(store.retailer)} allows {category.name_en}"
													checked={allowedFor(store.retailer, category.id)}
													onchange={(event) =>
														setAllowed(
															store.retailer,
															category.id,
															null,
															(event.currentTarget as HTMLInputElement).checked
														)}
												/>
												<span>{category.name_en}</span>
											</label>
											{#if subcategoriesOf(category.id).length > 0}
												<ul class="subcategory-list">
													{#each subcategoriesOf(category.id) as subcategory (subcategory.id)}
														<li>
															<label class="rule">
																<input
																	type="checkbox"
																	aria-label="{retailerLabel(store.retailer)} allows {subcategory.name_en}"
																	checked={allowedFor(
																		store.retailer,
																		category.id,
																		subcategory.id
																	)}
																	onchange={(event) =>
																		setAllowed(
																			store.retailer,
																			category.id,
																			subcategory.id,
																			(event.currentTarget as HTMLInputElement).checked
																		)}
																/>
																<span>{subcategory.name_en}</span>
															</label>
														</li>
													{/each}
												</ul>
											{/if}
										</li>
									{/each}
								</ul>
							</details>
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

		<section class="data-controls" aria-label="Your data">
			<h2>Your data</h2>
			{#if householdStatus === 'loading'}
				<p class="hint" role="status">Loading your household…</p>
			{:else if householdStatus === 'error'}
				<p class="error" role="alert">Couldn’t load your household details.</p>
			{:else if household}
				<p class="hint">
					Download everything BuyWise holds for <strong>{household.name}</strong>, or delete it.
				</p>
				<button type="button" class="secondary" onclick={downloadExport} disabled={exporting}>
					{exporting ? 'Preparing…' : 'Download export'}
				</button>
				{#if exportError}
					<p class="error" role="alert">{exportError}</p>
				{/if}

				<div class="danger-zone">
					<h3>Delete household data</h3>
					<p class="hint">
						This permanently removes your household’s records from the live server: lists,
						catalogue, matches, preferences and purchase history. Backups expire on their own
						schedule (up to about a day), aggregate usage counters with no household id remain,
						and clearing any copies left on disk is an operator step.
					</p>
					<form class="delete-form" onsubmit={removeHousehold}>
						<label for="confirm-name">
							Type the household name (<strong>{household.name}</strong>) to confirm
						</label>
						<input id="confirm-name" type="text" autocomplete="off" bind:value={confirmName} />
						<label for="confirm-password">Your password</label>
						<input
							id="confirm-password"
							type="password"
							autocomplete="current-password"
							bind:value={confirmPassword}
						/>
						{#if deleteError}
							<p class="error" role="alert">{deleteError}</p>
						{/if}
						<button class="danger" type="submit" disabled={deleting || !canDelete}>
							{deleting ? 'Deleting…' : 'Delete household data'}
						</button>
					</form>
				</div>
			{/if}
		</section>
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
		flex-direction: column;
		gap: 0.5rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.6rem 0.75rem;
	}
	.store-controls {
		display: flex;
		align-items: center;
		gap: 0.75rem;
	}
	.store-name {
		flex: 1 1 auto;
		font-weight: 600;
	}
	.store-rules summary {
		cursor: pointer;
		font-size: 0.9rem;
		color: #0f766e;
	}
	.rules-hint {
		margin: 0.5rem 0;
	}
	.category-list,
	.subcategory-list {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.category-list {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}
	.category-item {
		font-size: 0.9rem;
	}
	.subcategory-list {
		margin-left: 1.4rem;
		margin-top: 0.3rem;
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.rule {
		display: flex;
		align-items: center;
		gap: 0.4rem;
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
	.data-controls {
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		border-top: 1px solid #e2e8f0;
		padding-top: 1.25rem;
	}
	.secondary {
		align-self: flex-start;
		padding: 0.5rem 1.1rem;
		border: 1px solid #0f766e;
		border-radius: 0.5rem;
		background: #fff;
		color: #0f766e;
		font-size: 0.95rem;
		cursor: pointer;
	}
	.secondary:disabled {
		opacity: 0.6;
		cursor: default;
	}
	.danger-zone {
		border: 1px solid #fecaca;
		border-radius: 0.5rem;
		padding: 0.75rem 1rem;
		background: #fff7f7;
	}
	.danger-zone h3 {
		font-size: 1rem;
		margin: 0 0 0.35rem;
		color: #991b1b;
	}
	.delete-form {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.delete-form label {
		font-size: 0.9rem;
	}
	.delete-form input {
		padding: 0.35rem 0.5rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
	}
	.danger {
		align-self: flex-start;
		padding: 0.5rem 1.1rem;
		border: none;
		border-radius: 0.5rem;
		background: #b91c1c;
		color: #fff;
		font-size: 0.95rem;
		cursor: pointer;
	}
	.danger:disabled {
		opacity: 0.5;
		cursor: default;
	}
</style>
