<script lang="ts">
	// Household receipt photos (issue #149). Members upload photos of household
	// receipts, see them listed with their retention expiry, open one full size,
	// and delete one with an inline confirmation.
	//
	// Like the other (app) views this page is intrinsically online: it reads
	// `GET /api/household` (for the id) then `GET /api/household/{id}/receipts` on
	// mount. SSR/prerender are disabled globally (src/routes/+layout.ts), so the
	// on-mount fetch only runs in the browser. The upload posts the chosen file's
	// raw bytes (never JSON), and every mutation is CSRF-protected.

	import {
		fetchHousehold,
		HttpResponseError,
		redirectToSignIn,
		type Household
	} from '$lib/api/household';
	import {
		deleteReceipt,
		listReceipts,
		receiptImageUrl,
		uploadReceipt,
		type Receipt
	} from '$lib/api/receipts';

	type Status = 'loading' | 'ready' | 'error';

	let status = $state<Status>('loading');
	let household = $state<Household | null>(null);
	let receipts = $state<Receipt[]>([]);

	// Upload control: the chosen file, the in-flight flag, and any failure.
	let chosenFile = $state<File | null>(null);
	/** The file input, so its value can be cleared after a successful upload. */
	let fileInput = $state<HTMLInputElement | null>(null);
	let uploading = $state(false);
	let uploadError = $state('');

	/** The receipt whose deletion is awaiting confirmation, if any. */
	let confirmingRemoval = $state<Receipt | null>(null);
	let removing = $state(false);
	/** The failure from the receipt currently awaiting confirmation. */
	let removeError = $state('');

	/**
	 * Load the household and its receipts. On any failure we surface a visible
	 * error and a retry rather than failing silently; the message stays generic
	 * because the endpoint's error bodies aren't a stable contract.
	 */
	async function load() {
		status = 'loading';
		try {
			household = await fetchHousehold();
			receipts = await listReceipts(household.id);
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

	/** "2026-05-03T10:00:00Z" → "3 May 2026"; falls back to the date part. */
	function dateLabel(iso: string): string {
		const d = new Date(iso);
		if (Number.isNaN(d.getTime())) return iso.slice(0, 10);
		return d.toLocaleDateString('en-IE', { day: 'numeric', month: 'short', year: 'numeric' });
	}

	/** Remember the chosen file (and clear any previous failure). */
	function chooseFile(event: Event) {
		chosenFile = (event.currentTarget as HTMLInputElement).files?.[0] ?? null;
		uploadError = '';
	}

	/** Upload the chosen photo and prepend the returned receipt to the list. */
	async function upload(event: SubmitEvent) {
		event.preventDefault();
		if (!household || !chosenFile) return;
		uploadError = '';
		uploading = true;
		try {
			const receipt = await uploadReceipt(household.id, chosenFile);
			receipts = [receipt, ...receipts];
			chosenFile = null;
			if (fileInput) fileInput.value = '';
		} catch (err) {
			if (err instanceof HttpResponseError && err.status === 401) {
				redirectToSignIn();
				return;
			}
			if (err instanceof HttpResponseError && err.status === 413) {
				uploadError = 'That photo is too large (max 10 MB).';
			} else if (err instanceof HttpResponseError && err.status === 415) {
				uploadError = 'Only JPEG, PNG, WebP or HEIC photos are accepted.';
			} else if (err instanceof HttpResponseError && err.status === 409) {
				uploadError = "This household's receipt storage is full. Delete some receipts first.";
			} else if (err instanceof HttpResponseError && err.status === 501) {
				uploadError = 'External processing is not available yet.';
			} else {
				uploadError = 'Couldn’t upload the receipt. Check your connection and try again.';
			}
		} finally {
			uploading = false;
		}
	}

	/** Open the inline deletion confirmation for a receipt. */
	function askRemove(receipt: Receipt) {
		removeError = '';
		confirmingRemoval = receipt;
	}

	/** Close the inline deletion confirmation. */
	function cancelRemove() {
		confirmingRemoval = null;
		removeError = '';
	}

	/** Delete the receipt awaiting confirmation. */
	async function confirmRemove() {
		const receipt = confirmingRemoval;
		if (!household || !receipt) return;
		removeError = '';
		removing = true;
		try {
			await deleteReceipt(household.id, receipt.id);
			receipts = receipts.filter((r) => r.id !== receipt.id);
			confirmingRemoval = null;
		} catch (err) {
			if (err instanceof HttpResponseError && err.status === 401) {
				redirectToSignIn();
				return;
			}
			removeError = 'Couldn’t delete the receipt. Check your connection and try again.';
		} finally {
			removing = false;
		}
	}
</script>

<svelte:head>
	<title>BuyWise — Receipts</title>
</svelte:head>

<main class="receipts-view">
	<header class="receipts-header">
		<h1>Receipts</h1>
		<a class="back" href="/">Home</a>
	</header>

	{#if status === 'loading'}
		<p class="loading" role="status" aria-live="polite">Loading your receipts…</p>
	{:else if status === 'error'}
		<div class="error" role="alert">
			<p>Couldn’t load your receipts. Check your connection and try again.</p>
			<button type="button" class="retry" onclick={retry}>Try again</button>
		</div>
	{:else}
		<section class="upload" aria-label="Add a receipt">
			<h2>Add a receipt</h2>
			<p class="hint">
				Photos are stored privately on this household’s own server, kept for 30 days, and read
				on this server (local processing) only.
			</p>
			<form class="upload-form" onsubmit={upload}>
				<label for="receipt-file">Receipt photo</label>
				<input
					id="receipt-file"
					type="file"
					accept="image/jpeg,image/png,image/webp,image/heic,image/heif"
					capture="environment"
					onchange={chooseFile}
					bind:this={fileInput}
				/>
				<button class="save" type="submit" disabled={uploading || !chosenFile}>
					{uploading ? 'Uploading…' : 'Upload'}
				</button>
			</form>
			{#if uploadError}
				<p class="error" role="alert">{uploadError}</p>
			{/if}
		</section>

		<section class="receipts" aria-label="Receipts">
			<h2>Your receipts</h2>
			{#if receipts.length === 0}
				<p class="empty">
					No receipts yet. Upload a photo of a household receipt and it will appear here.
				</p>
			{:else}
				<ul class="receipt-list">
					{#each receipts as receipt (receipt.id)}
						<li class="receipt-row">
							<img
								class="thumb"
								src={receiptImageUrl(household!.id, receipt.id)}
								alt="Receipt from {dateLabel(receipt.uploaded_at)}"
								loading="lazy"
							/>
							<div class="receipt-who">
								<strong>{dateLabel(receipt.uploaded_at)}</strong>
								<span class="receipt-expiry">Expires {dateLabel(receipt.expires_at)}</span>
							</div>
							<div class="receipt-actions">
								<a
									class="view"
									href={receiptImageUrl(household!.id, receipt.id)}
									target="_blank"
									rel="noopener"
									aria-label="View receipt from {dateLabel(receipt.uploaded_at)}"
								>
									View
								</a>
								{#if confirmingRemoval?.id === receipt.id}
									<div class="remove-confirm">
										<p class="hint">Delete this receipt? It cannot be undone.</p>
										{#if removeError}
											<p class="error" role="alert">{removeError}</p>
										{/if}
										<button
											type="button"
											class="danger"
											onclick={confirmRemove}
											disabled={removing}
										>
											{removing ? 'Deleting…' : 'Confirm delete'}
										</button>
										<button
											type="button"
											class="secondary"
											onclick={cancelRemove}
											disabled={removing}
										>
											Cancel
										</button>
									</div>
								{:else}
									<button type="button" class="secondary" onclick={() => askRemove(receipt)}>
										Remove
									</button>
								{/if}
							</div>
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}
</main>

<style>
	.receipts-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}
	.receipts-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.receipts-header h1 {
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
	.retry {
		margin-top: 0.6rem;
		padding: 0.4rem 0.9rem;
		border: 1px solid #991b1b;
		background: white;
		color: #991b1b;
		border-radius: 0.4rem;
		cursor: pointer;
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
	.upload {
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.75rem 1rem;
	}
	.upload-form {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.75rem;
	}
	.upload-form label {
		font-size: 0.9rem;
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
	.empty {
		color: #64748b;
		font-size: 0.9rem;
	}
	.receipt-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.receipt-row {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.6rem 0.75rem;
	}
	.thumb {
		width: 3.5rem;
		height: 3.5rem;
		object-fit: cover;
		border-radius: 0.4rem;
		border: 1px solid #e2e8f0;
		background: #f8fafc;
	}
	.receipt-who {
		display: flex;
		flex-direction: column;
		flex: 1 1 auto;
	}
	.receipt-expiry {
		font-size: 0.8rem;
		color: #64748b;
	}
	.receipt-actions {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}
	.view {
		font-size: 0.85rem;
		font-weight: 600;
		color: #0f766e;
		white-space: nowrap;
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
	.remove-confirm {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		align-items: flex-start;
	}
	.remove-confirm .hint {
		margin: 0;
	}
</style>
