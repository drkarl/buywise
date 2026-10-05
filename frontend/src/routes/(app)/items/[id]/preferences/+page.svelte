<script lang="ts">
	// Per-item household product preferences (issue #102). Lets the household set,
	// from the app, how a canonical item may be matched: the constraint policy, the
	// brand rules (single brand, allowed set, excluded brands), a preferred size and
	// the substitution behaviour (including "ask before substituting"). It also
	// lists the item's known retailer products so acceptable alternatives are
	// visible and a rejected product can be restored or a suggested one rejected.
	//
	// The optimiser reads exactly these columns, so saving here changes the very
	// next optimisation. Accept/reject is written through the existing
	// `POST /api/match-feedback` path (P5-T1) and then re-read, so the warning that
	// no usable product exists reflects the new state.
	//
	// Like the settings/optimise views this page is intrinsically online; SSR is
	// disabled globally so the on-mount load only runs in the browser.

	import { page } from '$app/state';
	import {
		fetchPreferences,
		savePreferences,
		type ConstraintPolicy,
		type Preferences,
		type SizeBasis,
		type SubstitutionPolicy
	} from '$lib/api/preferences';
	import { retailerLabel, sendMatchFeedback } from '$lib/api/optimise';

	const canonicalItemId = $derived(page.params.id ?? '');
	/** The list to return to, threaded through from the list/optimise links. */
	const returnListId = $derived(page.url.searchParams.get('list') ?? '');

	type Status = 'loading' | 'ready' | 'error';

	let status = $state<Status>('loading');
	let prefs = $state<Preferences | null>(null);
	let itemName = $state('');
	let loadFailed = $state(false);

	// Editable form state, applied from the loaded preferences.
	let policy = $state<ConstraintPolicy>('any');
	let brand = $state('');
	let allowedBrandsText = $state('');
	let excludedBrandsText = $state('');
	let sizeValue = $state<number | null>(null);
	let sizeBasis = $state<SizeBasis | ''>('');
	let substitution = $state<SubstitutionPolicy>('any');

	let saving = $state(false);
	let savedMessage = $state('');
	let saveError = $state('');
	/** Per-match busy flag while an accept/reject POST is in flight. */
	let matchBusy = $state<Set<string>>(new Set());

	const POLICY_OPTIONS: { value: ConstraintPolicy; label: string }[] = [
		{ value: 'any', label: 'Any product' },
		{ value: 'brand', label: 'A specific brand' },
		{ value: 'allowed_set', label: 'One of several brands' },
		{ value: 'own_label', label: 'Own-label only' },
		{ value: 'exact', label: 'The exact product' },
		{ value: 'ask', label: 'Ask me each time' }
	];

	const SUBSTITUTION_OPTIONS: { value: SubstitutionPolicy; label: string; hint: string }[] = [
		{ value: 'any', label: 'Substitute freely', hint: 'Any acceptable product may be chosen.' },
		{
			value: 'ask',
			label: 'Ask before substituting',
			hint: 'Hold the item out of plans until you confirm a product.'
		},
		{
			value: 'strict',
			label: 'Only the exact product',
			hint: 'Never substitute; the item is left out if the exact product has no price.'
		}
	];

	const SIZE_BASIS_OPTIONS: { value: SizeBasis; label: string }[] = [
		{ value: 'g', label: 'grams' },
		{ value: 'ml', label: 'millilitres' },
		{ value: 'each', label: 'items' }
	];

	/** Human text for a stable warning code; unknown codes are shown verbatim. */
	function warningText(code: string): string {
		switch (code) {
			case 'brand_policy_missing_brand':
				return 'This item requires a brand, but none is set. It will be left out of plans until you choose one.';
			case 'allowed_set_empty':
				return 'This item allows a set of brands, but none are listed. It will be left out of plans until you add one.';
			case 'no_usable_matches':
				return 'No acceptable product is known for this item yet, so your rules cannot be checked. It will be left out of plans until a product is matched.';
			default:
				return code;
		}
	}

	/** Split a comma/newline separated brand list, trimming and dropping blanks. */
	function parseBrandList(text: string): string[] {
		return text
			.split(/[\n,]/)
			.map((s) => s.trim())
			.filter((s) => s.length > 0);
	}

	/** Apply a loaded preferences payload to the form. */
	function applyPreferences(next: Preferences) {
		prefs = next;
		itemName = next.name_en || next.name_es;
		policy = next.default_constraint_policy;
		brand = next.constraints.brand ?? '';
		allowedBrandsText = next.constraints.allowed_brands.join(', ');
		excludedBrandsText = next.constraints.excluded_brands.join(', ');
		sizeValue = next.constraints.preferred_size_value;
		sizeBasis = next.constraints.preferred_size_basis ?? '';
		substitution = next.constraints.substitution_policy;
	}

	async function load() {
		if (!canonicalItemId) return;
		status = 'loading';
		loadFailed = false;
		try {
			applyPreferences(await fetchPreferences(canonicalItemId));
			status = 'ready';
		} catch {
			loadFailed = true;
			status = 'error';
		}
	}

	$effect(() => {
		load();
	});

	function retry() {
		load();
	}

	/** Accept or reject a known product; re-read so warnings reflect the change. */
	async function decideMatch(retailerProductId: string, action: 'accept' | 'reject') {
		if (matchBusy.has(retailerProductId)) return;
		matchBusy = new Set(matchBusy).add(retailerProductId);
		try {
			const { ok } = await sendMatchFeedback({
				canonicalItemId,
				retailerProductId,
				action
			});
			if (ok) {
				applyPreferences(await fetchPreferences(canonicalItemId));
			} else {
				saveError = 'Couldn’t save your product choice. Please try again.';
			}
		} catch {
			saveError = 'Couldn’t reach the server. Check your connection and try again.';
		} finally {
			const next = new Set(matchBusy);
			next.delete(retailerProductId);
			matchBusy = next;
		}
	}

	/** Save the whole preference set; the form always sends the complete state. */
	async function save(event: SubmitEvent) {
		event.preventDefault();
		savedMessage = '';
		saveError = '';

		const allowed =
			policy === 'allowed_set' ? parseBrandList(allowedBrandsText) : [];
		const excluded = parseBrandList(excludedBrandsText);
		const trimmedBrand = brand.trim();

		if (policy === 'brand' && trimmedBrand.length === 0) {
			saveError = 'Choose a brand, or pick a different rule.';
			return;
		}
		if (policy === 'allowed_set' && allowed.length === 0) {
			saveError = 'List at least one brand, or pick a different rule.';
			return;
		}

		let preferredSizeValue: number | null = null;
		let preferredSizeBasis: SizeBasis | null = null;
		if (sizeValue !== null) {
			if (!Number.isFinite(sizeValue) || sizeValue <= 0) {
				saveError = 'Enter a size greater than zero, or leave it blank for any size.';
				return;
			}
			if (sizeBasis === '') {
				saveError = 'Choose a unit for the preferred size (grams, millilitres or items).';
				return;
			}
			preferredSizeValue = sizeValue;
			preferredSizeBasis = sizeBasis;
		}

		saving = true;
		try {
			const updated = await savePreferences(canonicalItemId, {
				default_constraint_policy: policy,
				brand: trimmedBrand.length > 0 ? trimmedBrand : null,
				allowed_brands: allowed,
				excluded_brands: excluded,
				preferred_size_value: preferredSizeValue,
				preferred_size_basis: preferredSizeBasis,
				substitution_policy: substitution
			});
			applyPreferences(updated);
			savedMessage = 'Saved. Your next optimisation will use these preferences.';
		} catch {
			saveError = 'Couldn’t save your preferences. Check your connection and try again.';
		} finally {
			saving = false;
		}
	}
</script>

<svelte:head>
	<title>BuyWise — Item preferences</title>
</svelte:head>

<main class="preferences-view">
	<header class="preferences-header">
		<h1>{itemName || 'Item preferences'}</h1>
		{#if returnListId}
			<a class="back" href="/list/{returnListId}">Back to list</a>
		{:else}
			<a class="back" href="/">Home</a>
		{/if}
	</header>

	{#if status === 'loading'}
		<p class="loading" role="status" aria-live="polite">Loading preferences…</p>
	{:else if status === 'error'}
		<div class="error" role="alert">
			<p>
				{loadFailed
					? 'Couldn’t load these preferences. Check your connection and try again.'
					: 'This item wasn’t found.'}
			</p>
			<button type="button" class="retry" onclick={retry}>Try again</button>
		</div>
	{:else if prefs}
		{#if prefs.warnings.length > 0}
			<ul class="warnings" aria-label="Things to fix">
				{#each prefs.warnings as code (code)}
					<li class="warning" role="status">{warningText(code)}</li>
				{/each}
			</ul>
		{/if}

		<form class="preferences-form" onsubmit={save}>
			<section aria-label="Item rule">
				<h2>How should this item be chosen?</h2>
				<label class="field" for="policy">Rule</label>
				<select id="policy" bind:value={policy}>
					{#each POLICY_OPTIONS as option (option.value)}
						<option value={option.value}>{option.label}</option>
					{/each}
				</select>

				{#if policy === 'brand'}
					<label class="field" for="brand">Brand</label>
					<input id="brand" type="text" bind:value={brand} placeholder="e.g. Heinz" />
				{/if}

				{#if policy === 'allowed_set'}
					<label class="field" for="allowed">Allowed brands</label>
					<input
						id="allowed"
						type="text"
						bind:value={allowedBrandsText}
						placeholder="e.g. Heinz, Branston"
					/>
					<p class="hint">Separate brands with commas.</p>
				{/if}
			</section>

			<section aria-label="Excluded brands">
				<h2>Brands to avoid</h2>
				<label class="field" for="excluded">Never choose these brands</label>
				<input
					id="excluded"
					type="text"
					bind:value={excludedBrandsText}
					placeholder="e.g. Branston"
				/>
				<p class="hint">Separate brands with commas. Applies whatever the rule above.</p>
			</section>

			<section aria-label="Preferred size">
				<h2>Preferred size</h2>
				<div class="size-row">
					<label class="field" for="size-value">Size</label>
					<input
						id="size-value"
						type="number"
						min="0"
						step="any"
						bind:value={sizeValue}
						placeholder="Any"
					/>
					<select id="size-basis" aria-label="Size unit" bind:value={sizeBasis}>
						<option value="">unit</option>
						{#each SIZE_BASIS_OPTIONS as option (option.value)}
							<option value={option.value}>{option.label}</option>
						{/each}
					</select>
				</div>
				<p class="hint">
					Products within 20% of this size qualify. Leave blank to accept any size.
				</p>
			</section>

			<section aria-label="Substitutions">
				<h2>Substitutions</h2>
				{#each SUBSTITUTION_OPTIONS as option (option.value)}
					<label class="substitution">
						<input
							type="radio"
							name="substitution"
							value={option.value}
							checked={substitution === option.value}
							onchange={() => (substitution = option.value)}
						/>
						<span class="substitution-label">{option.label}</span>
						<span class="hint">{option.hint}</span>
					</label>
				{/each}
			</section>

			{#if saveError}
				<p class="error" role="alert">{saveError}</p>
			{:else if savedMessage}
				<p class="saved" role="status">{savedMessage}</p>
			{/if}

			<button class="save" type="submit" disabled={saving}>
				{saving ? 'Saving…' : 'Save preferences'}
			</button>
		</form>

		<section class="products" aria-label="Products">
			<h2>Known products</h2>
			{#if prefs.matches.length === 0}
				<p class="hint">
					No products are matched to this item yet, so your rules cannot be checked until one
					is.
				</p>
			{:else}
				<p class="hint">
					Reject a product to keep it out of every plan; restore one to allow it again.
				</p>
				<ul class="product-list">
					{#each prefs.matches as match (match.retailer_product_id)}
						<li class="product" class:rejected={match.status === 'rejected'}>
							<span class="product-detail">
								<span class="product-name">{match.name}</span>
								{#if match.brand}<span class="product-meta">{match.brand}</span>{/if}
								{#if match.size_text}<span class="product-meta">{match.size_text}</span>{/if}
								<span class="product-meta">{retailerLabel(match.retailer)}</span>
								<span class="product-status">
									{match.status === 'rejected'
										? 'Rejected'
										: match.status === 'pending'
											? 'Awaiting review'
											: 'Allowed'}
								</span>
							</span>
							{#if match.status === 'rejected'}
								<button
									type="button"
									class="restore"
									disabled={matchBusy.has(match.retailer_product_id)}
									onclick={() => decideMatch(match.retailer_product_id, 'accept')}>Restore</button
								>
							{:else}
								<button
									type="button"
									class="reject"
									disabled={matchBusy.has(match.retailer_product_id)}
									onclick={() => decideMatch(match.retailer_product_id, 'reject')}>Reject</button
								>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}
</main>

<style>
	.preferences-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.preferences-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.preferences-header h1 {
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
		padding: 0.75rem 1rem;
		border: 1px solid #fca5a5;
		border-radius: 0.5rem;
		background: #fef2f2;
		color: #b91c1c;
	}
	.warnings {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.warning {
		padding: 0.6rem 0.8rem;
		border: 1px solid #f59e0b;
		border-radius: 0.5rem;
		background: #fffbeb;
		color: #92400e;
		font-size: 0.9rem;
	}
	.preferences-form {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.preferences-form section {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.8rem;
	}
	.preferences-form h2 {
		font-size: 1rem;
		margin: 0 0 0.2rem;
	}
	.field {
		font-size: 0.85rem;
		color: #475569;
	}
	input[type='text'],
	input[type='number'],
	select {
		padding: 0.45rem 0.55rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
		font-size: 0.95rem;
	}
	.size-row {
		display: flex;
		gap: 0.5rem;
		align-items: center;
	}
	.size-row input {
		flex: 1;
	}
	.hint {
		margin: 0;
		font-size: 0.8rem;
		color: #64748b;
	}
	.substitution {
		display: grid;
		grid-template-columns: auto 1fr;
		grid-template-rows: auto auto;
		column-gap: 0.5rem;
		align-items: center;
	}
	.substitution input {
		grid-row: 1 / span 2;
	}
	.substitution-label {
		font-size: 0.95rem;
	}
	.saved {
		color: #0f766e;
		font-size: 0.9rem;
	}
	.save {
		align-self: flex-start;
		padding: 0.55rem 1rem;
		border: none;
		border-radius: 0.45rem;
		background: #0f766e;
		color: #fff;
		font-size: 0.95rem;
		cursor: pointer;
	}
	.save:disabled {
		opacity: 0.6;
		cursor: not-allowed;
	}
	.product-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.product {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
		padding: 0.5rem 0;
		border-bottom: 1px solid #e2e8f0;
	}
	.product.rejected {
		opacity: 0.6;
	}
	.product-detail {
		display: flex;
		flex-wrap: wrap;
		gap: 0.4rem;
		align-items: baseline;
	}
	.product-name {
		font-size: 0.95rem;
	}
	.product-meta {
		font-size: 0.8rem;
		color: #64748b;
	}
	.product-status {
		font-size: 0.75rem;
		text-transform: uppercase;
		letter-spacing: 0.03em;
		color: #64748b;
	}
	.reject,
	.restore {
		flex-shrink: 0;
		padding: 0.3rem 0.6rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.4rem;
		background: #f8fafc;
		cursor: pointer;
	}
</style>
