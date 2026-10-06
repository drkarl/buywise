<script lang="ts">
	// Purchases (issue #156, roadmap #114): the household's purchase ledger, with
	// purchases added and edited by hand beside the photo of the receipt.
	//
	// Like the other (app) views this page is intrinsically online: it reads
	// `GET /api/household` (for the id) then the ledger and the household's receipt
	// photos. SSR/prerender are disabled globally (src/routes/+layout.ts), so the
	// on-mount fetch only runs in the browser. Money is integer cents on the wire;
	// the euro amounts typed here are converted by `$lib/purchases/money`, never by
	// floating-point arithmetic.
	//
	// The form shows the running sum of its lines beside the printed receipt total
	// and warns when they differ, but the warning never blocks a save: a receipt
	// can legitimately disagree with the lines a member typed. Photo linking is
	// part of the form and, on its own, is available from a purchase's detail view.

	import { tick } from 'svelte';
	import { formatEuros, retailerLabel } from '$lib/api/optimise';
	import {
		fetchHousehold,
		HttpResponseError,
		redirectToSignIn,
		type Household
	} from '$lib/api/household';
	import {
		createManualPurchase,
		deletePurchase,
		getPurchase,
		listPurchases,
		PurchaseValidationError,
		setPurchaseReceipt,
		updateManualPurchase,
		type PurchaseDetail,
		type PurchaseInput,
		type PurchaseLine,
		type PurchaseSummary
	} from '$lib/api/purchases';
	import { listReceipts, receiptImageUrl, uploadReceipt, type Receipt } from '$lib/api/receipts';
	import { parseEurosToCents, toEurosInput } from '$lib/purchases/money';

	type Status = 'loading' | 'ready' | 'error';
	type View = 'list' | 'form' | 'detail';
	type Tab = 'details' | 'photo';

	/** The common Irish retailers a free-text field suggests, alongside the
	 * household's own previous ones. The token is normalised by the server. */
	const COMMON_RETAILERS = ['tesco', 'dunnes', 'supervalu', 'aldi', 'lidl'];

	const MONTHS = [
		'Jan',
		'Feb',
		'Mar',
		'Apr',
		'May',
		'Jun',
		'Jul',
		'Aug',
		'Sep',
		'Oct',
		'Nov',
		'Dec'
	];

	/** One editable line of the form (amounts as typed euro strings). */
	interface LineDraft {
		key: number;
		description: string;
		quantity: string;
		lineTotal: string;
		discount: string;
	}

	let status = $state<Status>('loading');
	let household = $state<Household | null>(null);
	let purchases = $state<PurchaseSummary[]>([]);
	let receipts = $state<Receipt[]>([]);

	let view = $state<View>('list');

	// The add/edit form.
	let formId = $state<string | null>(null);
	let retailer = $state('');
	let purchaseDate = $state('');
	let receiptTotal = $state('');
	let receiptImageId = $state<string | null>(null);
	let lines = $state<LineDraft[]>([]);
	let formErrors = $state<string[]>([]);
	let saving = $state(false);
	/** Which panel the phone tabs show; ignored on a wide screen, where both show. */
	let mobileTab = $state<Tab>('details');
	let zoomed = $state(false);

	// Uploading a photo from the form.
	let fileInput = $state<HTMLInputElement | null>(null);
	let chosenFile = $state<File | null>(null);
	let uploading = $state(false);
	let photoError = $state('');

	// One purchase's detail.
	let detail = $state<PurchaseDetail | null>(null);
	let detailStatus = $state<Status>('loading');
	let detailError = $state('');
	let confirmingDelete = $state(false);
	let deleting = $state(false);

	let nextLineKey = 0;

	/** The household's ledger, newest purchase first. */
	let ordered = $derived(
		[...purchases].sort((a, b) =>
			a.purchase_date === b.purchase_date
				? a.id < b.id
					? 1
					: -1
				: a.purchase_date < b.purchase_date
					? 1
					: -1
		)
	);

	/** Free-text suggestions: the household's own retailers plus the common ones. */
	let retailerSuggestions = $derived([
		...new Set([...COMMON_RETAILERS, ...purchases.map((p) => p.retailer)])
	]);

	/** The form's lines with their typed amounts parsed to integer cents. */
	let rows = $derived(
		lines.map((line) => {
			const totalCents = parseEurosToCents(line.lineTotal);
			const discountRaw = line.discount.trim();
			const discountCents = discountRaw === '' ? 0 : parseEurosToCents(discountRaw);
			const quantityRaw = line.quantity.trim().replace(',', '.');
			const quantity = quantityRaw === '' ? null : Number(quantityRaw);
			return {
				key: line.key,
				description: line.description,
				quantity,
				quantityValid: quantity === null || (Number.isFinite(quantity) && quantity > 0),
				totalCents,
				discountCents
			};
		})
	);

	/** The running sum of the lines: each line total less its discount. */
	let lineSumCents = $derived(
		rows.reduce((sum, row) => sum + (row.totalCents ?? 0) - (row.discountCents ?? 0), 0)
	);

	/** True when a line's amount cannot be read as money (so the sum is partial). */
	let lineMoneyInvalid = $derived(
		rows.some((row) => row.totalCents === null || row.totalCents < 0 || row.discountCents === null)
	);

	/** The printed receipt total, or `null` when the field is left blank. */
	let receiptTotalCents = $derived(
		receiptTotal.trim() === '' ? null : parseEurosToCents(receiptTotal)
	);

	let receiptTotalInvalid = $derived(
		receiptTotal.trim() !== '' && (receiptTotalCents === null || receiptTotalCents < 0)
	);

	/** The lines and the printed total disagree; saving is still allowed. */
	let mismatch = $derived(
		!receiptTotalInvalid &&
			!lineMoneyInvalid &&
			receiptTotalCents !== null &&
			receiptTotalCents !== lineSumCents
	);

	function findReceipt(id: string | null): Receipt | null {
		return id === null ? null : (receipts.find((receipt) => receipt.id === id) ?? null);
	}

	let linkedReceipt = $derived(findReceipt(receiptImageId));
	let detailLinkedReceipt = $derived(findReceipt(detail?.receipt_image_id ?? null));

	async function load() {
		status = 'loading';
		try {
			household = await fetchHousehold();
			purchases = await listPurchases(household.id);
			receipts = await listReceipts(household.id);
			status = 'ready';
		} catch {
			status = 'error';
		}
	}

	$effect(() => {
		load();
	});

	async function refreshList() {
		if (!household) return;
		purchases = await listPurchases(household.id);
	}

	/** "2026-10-01" → "1 Oct 2026", without a timezone shift. */
	function dateLabel(value: string): string {
		const [year, month, day] = value
			.slice(0, 10)
			.split('-')
			.map(Number);
		if (!year || !month || !day) return value.slice(0, 10);
		return `${day} ${MONTHS[month - 1]} ${year}`;
	}

	/** Today's local calendar date as `YYYY-MM-DD`. */
	function todayIso(): string {
		const now = new Date();
		const local = new Date(now.getTime() - now.getTimezoneOffset() * 60000);
		return local.toISOString().slice(0, 10);
	}

	function newBlankLine(): LineDraft {
		return { key: nextLineKey++, description: '', quantity: '1', lineTotal: '', discount: '' };
	}

	function startAdd() {
		formId = null;
		retailer = '';
		purchaseDate = todayIso();
		receiptTotal = '';
		receiptImageId = null;
		lines = [newBlankLine()];
		formErrors = [];
		mobileTab = 'details';
		zoomed = false;
		chosenFile = null;
		photoError = '';
		view = 'form';
	}

	function startEdit(record: PurchaseDetail) {
		formId = record.id;
		retailer = retailerLabel(record.retailer);
		purchaseDate = record.purchase_date;
		let payloadTotal: unknown;
		let payloadReadable = false;
		try {
			const payload: unknown = JSON.parse(record.source_payload);
			payloadReadable = true;
			if (payload !== null && typeof payload === 'object' && !Array.isArray(payload)) {
				payloadTotal = (payload as Record<string, unknown>).total_cents;
			}
		} catch {
			payloadReadable = false;
		}
		receiptTotal = payloadReadable
			? typeof payloadTotal === 'number'
				? toEurosInput(payloadTotal)
				: ''
			: toEurosInput(record.total_cents);
		receiptImageId = findReceipt(record.receipt_image_id) ? record.receipt_image_id : null;
		lines = record.lines.map((line) => ({
			key: nextLineKey++,
			description: line.original_text,
			quantity: line.quantity === null ? '' : String(line.quantity),
			lineTotal: toEurosInput(line.line_total_cents),
			discount: line.line_discount_cents === 0 ? '' : toEurosInput(line.line_discount_cents)
		}));
		if (lines.length === 0) lines = [newBlankLine()];
		formErrors = [];
		mobileTab = 'details';
		zoomed = false;
		chosenFile = null;
		photoError = '';
		view = 'form';
	}

	async function openDetail(id: string) {
		view = 'detail';
		detailStatus = 'loading';
		detailError = '';
		confirmingDelete = false;
		zoomed = false;
		try {
			detail = household ? await getPurchase(household.id, id) : null;
			detailStatus = 'ready';
		} catch {
			detailStatus = 'error';
		}
	}

	/** Add one blank line below the last and move focus into it. */
	function addLine() {
		const line = newBlankLine();
		lines = [...lines, line];
		void tick().then(() => document.getElementById(`line-desc-${line.key}`)?.focus());
	}

	/** Enter on the last line's fields adds the next line, for keyboard entry. */
	function onLineKeydown(event: KeyboardEvent, index: number) {
		if (event.key === 'Enter' && index === lines.length - 1) {
			event.preventDefault();
			addLine();
		}
	}

	function removeLine(key: number) {
		lines = lines.filter((line) => line.key !== key);
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!household) return;

		const problems: string[] = [];
		if (retailer.trim() === '') problems.push('Enter the retailer.');
		if (!/^\d{4}-\d{2}-\d{2}$/.test(purchaseDate)) problems.push('Enter the purchase date.');
		if (receiptTotalInvalid) {
			problems.push('The receipt total must be an amount of €0.00 or more.');
		}
		if (lineMoneyInvalid) {
			problems.push('Enter a line total, and a discount, as amounts (the discount may be negative).');
		}
		if (rows.some((row) => row.description.trim() === '' || !row.quantityValid)) {
			problems.push('Every line needs a description and, if given, a positive quantity.');
		}
		if (problems.length > 0) {
			formErrors = problems;
			return;
		}

		const input: PurchaseInput = {
			retailer: retailer.trim(),
			purchase_date: purchaseDate,
			total_cents: receiptTotalCents,
			receipt_image_id: receiptImageId,
			lines: rows.map((row) => ({
				original_text: row.description.trim(),
				quantity: row.quantity,
				line_total_cents: row.totalCents ?? 0,
				line_discount_cents: row.discountCents ?? 0
			}))
		};

		formErrors = [];
		saving = true;
		let saved: PurchaseDetail;
		try {
			saved =
				formId === null
					? await createManualPurchase(household.id, input)
				: await updateManualPurchase(household.id, formId, input);
		} catch (err) {
			saving = false;
			if (err instanceof HttpResponseError && err.status === 401) {
				redirectToSignIn();
				return;
			}
			formErrors =
				err instanceof PurchaseValidationError && err.messages.length > 0
					? err.messages
					: ['Couldn’t save the purchase. Check your connection and try again.'];
			return;
		}
		saving = false;
		detail = saved;
		detailStatus = 'ready';
		detailError = '';
		confirmingDelete = false;
		zoomed = false;
		view = 'detail';
		try {
			await refreshList();
		} catch {
			detailError = 'Purchase saved, but the list could not be refreshed. Try again later.';
		}
	}

	async function confirmDelete() {
		if (!household || !detail) return;
		detailError = '';
		deleting = true;
		try {
			await deletePurchase(household.id, detail.id);
			detail = null;
			confirmingDelete = false;
			await refreshList();
			view = 'list';
		} catch (err) {
			if (err instanceof HttpResponseError && err.status === 401) {
				redirectToSignIn();
				return;
			}
			detailError = 'Couldn’t delete the purchase. Check your connection and try again.';
		} finally {
			deleting = false;
		}
	}

	/** Clear a purchase's photo link from the detail view. */
	async function unlinkPhoto() {
		if (!household || !detail) return;
		detailError = '';
		try {
			detail = await setPurchaseReceipt(household.id, detail.id, null);
			await refreshList();
		} catch (err) {
			if (err instanceof HttpResponseError && err.status === 401) {
				redirectToSignIn();
				return;
			}
			detailError = 'Couldn’t remove the photo link. Please try again.';
		}
	}

	function chooseFile(event: Event) {
		chosenFile = (event.currentTarget as HTMLInputElement).files?.[0] ?? null;
		photoError = '';
	}

	/** Upload the chosen photo and link it to this purchase. */
	async function uploadPhoto() {
		if (!household || !chosenFile) return;
		photoError = '';
		uploading = true;
		try {
			const receipt = await uploadReceipt(household.id, chosenFile);
			receipts = [receipt, ...receipts];
			receiptImageId = receipt.id;
			chosenFile = null;
			if (fileInput) fileInput.value = '';
		} catch (err) {
			if (err instanceof HttpResponseError && err.status === 401) {
				redirectToSignIn();
				return;
			}
			if (err instanceof HttpResponseError && err.status === 413) {
				photoError = 'That photo is too large (max 10 MB).';
			} else if (err instanceof HttpResponseError && err.status === 415) {
				photoError = 'Only JPEG, PNG, WebP or HEIC photos are accepted.';
			} else if (err instanceof HttpResponseError && err.status === 409) {
				photoError = 'This household’s receipt storage is full. Delete some receipts first.';
			} else {
				photoError = 'Couldn’t upload the photo. Check your connection and try again.';
			}
		} finally {
			uploading = false;
		}
	}

	/** The receipt photo for one line of the list, when a household id is known. */
	function linePhotoUrl(receiptId: string): string {
		return household ? receiptImageUrl(household.id, receiptId) : '';
	}

	/** The lines of the detail's current revision, newest-purchase first order kept. */
	function detailLineAmount(line: PurchaseLine): number {
		return line.line_total_cents - line.line_discount_cents;
	}
</script>

<svelte:head>
	<title>BuyWise — Purchases</title>
</svelte:head>

<main class="purchases-view">
	<header class="page-header">
		<h1>Purchases</h1>
		<a class="back" href="/">Home</a>
	</header>

	{#if status === 'loading'}
		<p class="loading" role="status" aria-live="polite">Loading your purchases…</p>
	{:else if status === 'error'}
		<div class="error-panel" role="alert">
			<p>Couldn’t load your purchases. Check your connection and try again.</p>
			<button type="button" class="retry" onclick={load}>Try again</button>
		</div>
	{:else if view === 'list'}
		<section class="list-panel" aria-label="Your purchases">
			<div class="list-head">
				<h2>Your purchases</h2>
				<button type="button" class="primary" onclick={startAdd}>Add purchase</button>
			</div>
			{#if ordered.length === 0}
				<p class="empty">
					No purchases yet. Add one by hand and it will appear here, ready to match against
					your receipt photo.
				</p>
			{:else}
				<ul class="purchase-list">
					{#each ordered as purchase (purchase.id)}
						<li class="purchase-row">
							<div class="purchase-main">
								<strong>{retailerLabel(purchase.retailer)}</strong>
								<span class="purchase-date">{dateLabel(purchase.purchase_date)}</span>
							</div>
							<div class="purchase-facts">
								<span class="total">{formatEuros(purchase.total_cents)}</span>
								<span class="lines">
									{purchase.line_count} {purchase.line_count === 1 ? 'line' : 'lines'}
								</span>
								{@const rowReceipt = findReceipt(purchase.receipt_image_id)}
								<span class="photo-flag" class:linked={rowReceipt !== null}>
									{rowReceipt ? 'Photo linked' : 'No photo'}
								</span>
							</div>
							<button type="button" class="secondary" onclick={() => openDetail(purchase.id)}>
								Open
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{:else if view === 'form'}
		<form class="form" onsubmit={save}>
			<div class="tabs" role="tablist" aria-label="Purchase and receipt photo">
				<button
					type="button"
					role="tab"
					aria-selected={mobileTab === 'details'}
					class:active={mobileTab === 'details'}
					onclick={() => (mobileTab = 'details')}
				>
					Purchase
				</button>
				<button
					type="button"
					role="tab"
					aria-selected={mobileTab === 'photo'}
					class:active={mobileTab === 'photo'}
					onclick={() => (mobileTab = 'photo')}
				>
					Receipt photo
				</button>
			</div>

			<div class="panels" data-tab={mobileTab}>
				<section class="details-panel">
					<h2>{formId === null ? 'Add a purchase' : 'Edit purchase'}</h2>

					{#if formErrors.length > 0}
						<div class="error-panel" role="alert">
							<ul>
								{#each formErrors as message (message)}
									<li>{message}</li>
								{/each}
							</ul>
						</div>
					{/if}

					<div class="field">
						<label for="retailer">Retailer</label>
						<input
							id="retailer"
							type="text"
							list="retailer-suggestions"
							autocomplete="off"
							placeholder="e.g. Tesco"
							bind:value={retailer}
						/>
						<datalist id="retailer-suggestions">
							{#each retailerSuggestions as suggestion (suggestion)}
								<option value={retailerLabel(suggestion)}></option>
							{/each}
						</datalist>
					</div>

					<div class="field">
						<label for="purchase-date">Purchase date</label>
						<input id="purchase-date" type="date" bind:value={purchaseDate} />
					</div>

					<div class="field">
						<label for="receipt-total">Receipt total (€, optional)</label>
						<input
							id="receipt-total"
							type="text"
							inputmode="decimal"
							placeholder="Leave blank to use the lines"
							bind:value={receiptTotal}
						/>
					</div>

					<div class="lines">
						<div class="lines-head">
							<h3>Lines</h3>
							<button type="button" class="secondary" onclick={addLine}>Add line</button>
						</div>
						{#each rows as row, index (row.key)}
							<fieldset class="line">
								<legend class="sr-only">Line {index + 1}</legend>
								<div class="field line-description">
									<label for="line-desc-{row.key}">Description</label>
									<input
										id="line-desc-{row.key}"
										type="text"
										placeholder="As printed on the receipt"
										bind:value={lines[index].description}
										onkeydown={(event) => onLineKeydown(event, index)}
									/>
								</div>
								<div class="line-numbers">
									<div class="field">
										<label for="line-qty-{row.key}">Quantity</label>
										<input
											id="line-qty-{row.key}"
											type="text"
											inputmode="decimal"
											bind:value={lines[index].quantity}
											onkeydown={(event) => onLineKeydown(event, index)}
										/>
									</div>
									<div class="field">
										<label for="line-total-{row.key}">Line total (€)</label>
										<input
											id="line-total-{row.key}"
											type="text"
											inputmode="decimal"
											placeholder="0.00"
											bind:value={lines[index].lineTotal}
											onkeydown={(event) => onLineKeydown(event, index)}
										/>
									</div>
									<div class="field">
										<label for="line-discount-{row.key}">Discount (€, optional)</label>
										<input
											id="line-discount-{row.key}"
											type="text"
											inputmode="decimal"
											placeholder="0.00"
											bind:value={lines[index].discount}
											onkeydown={(event) => onLineKeydown(event, index)}
										/>
									</div>
								</div>
								<button
									type="button"
									class="remove-line"
									onclick={() => removeLine(row.key)}
									disabled={rows.length === 1}
								>
									Remove line
								</button>
							</fieldset>
						{/each}
					</div>

					<div class="totals" aria-live="polite">
						<p class="sum">
							Lines add up to <strong>{formatEuros(lineSumCents)}</strong>{#if receiptTotalCents !== null && !receiptTotalInvalid}
								, receipt total {formatEuros(receiptTotalCents)}{/if}.
						</p>
						{#if mismatch}
							<p class="mismatch" role="status">
								The lines and the receipt total do not match. Check the amounts — you can
								still save.
							</p>
						{/if}
					</div>
				</section>

				<section class="photo-panel" aria-label="Receipt photo">
					<h2>Receipt photo</h2>
					{#if linkedReceipt}
						<figure class="photo">
							<img
								class:zoomed
								src={linePhotoUrl(linkedReceipt.id)}
								alt="Linked receipt"
							/>
							<figcaption>
								<button
									type="button"
									class="secondary"
									aria-pressed={zoomed}
									onclick={() => (zoomed = !zoomed)}
								>
									{zoomed ? 'Zoom out' : 'Zoom in'}
								</button>
								<a
									class="secondary"
									href={linePhotoUrl(linkedReceipt.id)}
									target="_blank"
									rel="noopener">Open full size</a
								>
								<button type="button" class="remove-line" onclick={() => (receiptImageId = null)}>
									Remove
								</button>
							</figcaption>
						</figure>
					{:else}
						<p class="empty">No photo linked yet.</p>
					{/if}

					{#if receipts.length > 0}
						<h3>Your receipt photos</h3>
						<ul class="photo-choices">
							{#each receipts as receipt (receipt.id)}
								<li>
									<button
										type="button"
										class="photo-choice"
										class:active={receipt.id === receiptImageId}
										onclick={() => (receiptImageId = receipt.id)}
									>
										<img
											src={linePhotoUrl(receipt.id)}
											alt="Receipt from {dateLabel(receipt.uploaded_at)}"
											loading="lazy"
										/>
										<span>{dateLabel(receipt.uploaded_at)}</span>
									</button>
								</li>
							{/each}
						</ul>
					{/if}

					<h3>Upload a photo</h3>
					<label for="purchase-photo">Receipt photo file</label>
					<input
						id="purchase-photo"
						type="file"
						accept="image/jpeg,image/png,image/webp,image/heic,image/heif"
						onchange={chooseFile}
						bind:this={fileInput}
					/>
					<button type="button" class="secondary" onclick={uploadPhoto} disabled={uploading || !chosenFile}>
						{uploading ? 'Uploading…' : 'Upload and link'}
					</button>
					{#if photoError}
						<p class="error-panel" role="alert">{photoError}</p>
					{/if}
				</section>
			</div>

			<div class="form-actions">
				<button type="submit" class="primary" disabled={saving}>
					{saving ? 'Saving…' : 'Save purchase'}
				</button>
				<button type="button" class="secondary" onclick={() => (view = 'list')}>Cancel</button>
			</div>
		</form>
	{:else if view === 'detail'}
		<section class="detail-panel" aria-label="Purchase">
			{#if detailStatus === 'loading'}
				<p class="loading" role="status" aria-live="polite">Loading the purchase…</p>
			{:else if detailStatus === 'error' || detail === null}
				<div class="error-panel" role="alert">
					<p>Couldn’t load this purchase.</p>
					<button type="button" class="retry" onclick={() => (view = 'list')}>Back to list</button>
				</div>
			{:else}
				{@const record = detail}
				<div class="detail-head">
					<h2>{retailerLabel(record.retailer)}</h2>
					<span class="purchase-date">{dateLabel(record.purchase_date)}</span>
				</div>
				<p class="detail-facts">
					<span class="total">{formatEuros(record.total_cents)}</span>
					·
					<span>{record.line_count} {record.line_count === 1 ? 'line' : 'lines'}</span>
					·
					<span>{record.source_kind === 'manual' ? 'Entered by hand' : 'Imported'}</span>
					{#if record.revision > 1}
						· <span>Revision {record.revision}</span>
					{/if}
				</p>

				{#if detailError}
					<p class="error-panel" role="alert">{detailError}</p>
				{/if}

				<h3>Lines</h3>
				<table class="detail-lines">
					<thead>
						<tr>
							<th scope="col">Description</th>
							<th scope="col">Qty</th>
							<th scope="col">Total</th>
						</tr>
					</thead>
					<tbody>
						{#each record.lines as line (line.id)}
							<tr>
								<td>{line.original_text}</td>
								<td>{line.quantity ?? ''}</td>
								<td>{formatEuros(detailLineAmount(line))}</td>
							</tr>
						{/each}
					</tbody>
				</table>

				<h3>Receipt photo</h3>
				{#if detailLinkedReceipt}
					<figure class="photo">
						<img
							class:zoomed
							src={linePhotoUrl(detailLinkedReceipt.id)}
							alt="Linked receipt"
						/>
						<figcaption>
							<button
								type="button"
								class="secondary"
								aria-pressed={zoomed}
								onclick={() => (zoomed = !zoomed)}
							>
								{zoomed ? 'Zoom out' : 'Zoom in'}
							</button>
							<a
								class="secondary"
								href={linePhotoUrl(detailLinkedReceipt.id)}
								target="_blank"
								rel="noopener">Open full size</a
							>
							<button type="button" class="remove-line" onclick={unlinkPhoto}>Remove photo</button>
						</figcaption>
					</figure>
				{:else}
					<p class="empty">
						No photo linked. Edit a hand-entered purchase to add one, or link one from the
						receipt’s own page.
					</p>
				{/if}

				<div class="form-actions">
					{#if record.source_kind === 'manual'}
						<button type="button" class="primary" onclick={() => startEdit(record)}>Edit</button>
					{/if}
					{#if confirmingDelete}
						<button type="button" class="danger" onclick={confirmDelete} disabled={deleting}>
							{deleting ? 'Deleting…' : 'Confirm delete'}
						</button>
						<button
							type="button"
							class="secondary"
							onclick={() => (confirmingDelete = false)}
							disabled={deleting}
						>
							Cancel
						</button>
					{:else}
						<button type="button" class="secondary" onclick={() => (confirmingDelete = true)}>
							Delete
						</button>
					{/if}
					<button type="button" class="secondary" onclick={() => (view = 'list')}>
						Back to list
					</button>
				</div>
				{#if confirmingDelete}
					<p class="hint">Delete this purchase? Its lines go with it. This cannot be undone.</p>
				{/if}
			{/if}
		</section>
	{/if}
</main>

<style>
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
		border: 0;
	}
	.purchases-view {
		max-width: 52rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}
	.page-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.page-header h1 {
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
	.error-panel {
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
	.list-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 1rem;
	}
	.list-head h2 {
		font-size: 1.1rem;
		margin: 0;
	}
	.empty {
		color: #64748b;
	}
	.purchase-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.purchase-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.75rem;
		flex-wrap: wrap;
		padding: 0.75rem 1rem;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
	}
	.purchase-main {
		display: flex;
		flex-direction: column;
	}
	.purchase-date {
		font-size: 0.85rem;
		color: #64748b;
	}
	.purchase-facts {
		display: flex;
		gap: 0.75rem;
		align-items: baseline;
		font-size: 0.9rem;
		color: #475569;
	}
	.total {
		font-weight: 600;
		color: #0f172a;
	}
	.photo-flag {
		font-size: 0.8rem;
		color: #94a3b8;
	}
	.photo-flag.linked {
		color: #0f766e;
		font-weight: 600;
	}
	.primary,
	.secondary,
	.danger,
	.remove-line {
		min-height: 2.75rem;
		padding: 0.6rem 1rem;
		border-radius: 0.5rem;
		font-size: 1rem;
		cursor: pointer;
	}
	.primary {
		border: 1px solid #0f766e;
		background: #0f766e;
		color: white;
	}
	.secondary {
		border: 1px solid #cbd5e1;
		background: white;
		color: #0f172a;
	}
	.danger,
	.remove-line {
		border: 1px solid #fecaca;
		background: #fef2f2;
		color: #991b1b;
	}
	.remove-line:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.form {
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}
	.tabs {
		display: flex;
		gap: 0.5rem;
	}
	.tabs button {
		flex: 1;
		min-height: 2.75rem;
		border: 1px solid #cbd5e1;
		background: white;
		border-radius: 0.5rem;
		font-size: 1rem;
	}
	.tabs button.active {
		background: #0f766e;
		border-color: #0f766e;
		color: white;
	}
	.panels {
		display: grid;
		gap: 1.25rem;
	}
	.details-panel,
	.photo-panel {
		display: flex;
		flex-direction: column;
		gap: 0.75rem;
	}
	.details-panel h2,
	.photo-panel h2 {
		font-size: 1.1rem;
		margin: 0;
	}
	.field {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}
	.field label,
	.photo-panel label {
		font-size: 0.9rem;
		color: #334155;
	}
	.field input,
	.photo-panel input[type='file'] {
		min-height: 2.75rem;
		padding: 0.5rem 0.7rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.5rem;
		font-size: 1rem;
	}
	.lines-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
	}
	.lines-head h3 {
		margin: 0;
		font-size: 1rem;
	}
	.line {
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.75rem;
		margin: 0 0 0.6rem;
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
	}
	.line-numbers {
		display: grid;
		gap: 0.6rem;
	}
	.totals {
		padding: 0.75rem 1rem;
		background: #f8fafc;
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
	}
	.totals p {
		margin: 0;
	}
	.mismatch {
		margin-top: 0.4rem !important;
		color: #92400e;
	}
	.photo {
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.photo img {
		max-width: 100%;
		border-radius: 0.5rem;
		border: 1px solid #e2e8f0;
	}
	.photo img.zoomed {
		transform: scale(2);
		transform-origin: top left;
	}
	.photo figcaption {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.photo-choices {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.photo-choice {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 0.25rem;
		padding: 0.4rem;
		border: 1px solid #cbd5e1;
		border-radius: 0.5rem;
		background: white;
		cursor: pointer;
	}
	.photo-choice.active {
		border-color: #0f766e;
		box-shadow: 0 0 0 2px #99f6e4;
	}
	.photo-choice img {
		width: 4rem;
		height: 4rem;
		object-fit: cover;
		border-radius: 0.25rem;
	}
	.photo-choice span {
		font-size: 0.75rem;
		color: #475569;
	}
	.form-actions {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}
	.detail-head {
		display: flex;
		align-items: baseline;
		gap: 0.6rem;
	}
	.detail-head h2 {
		margin: 0;
		font-size: 1.2rem;
	}
	.detail-facts {
		margin: 0;
		color: #475569;
	}
	.detail-lines {
		width: 100%;
		border-collapse: collapse;
	}
	.detail-lines th,
	.detail-lines td {
		text-align: left;
		padding: 0.5rem;
		border-bottom: 1px solid #e2e8f0;
	}
	.detail-lines td:last-child,
	.detail-lines th:last-child {
		text-align: right;
	}
	.hint {
		color: #92400e;
	}
	@media (min-width: 48rem) {
		.tabs {
			display: none;
		}
		.panels {
			grid-template-columns: minmax(0, 1fr) 20rem;
			align-items: start;
		}
		.line-numbers {
			grid-template-columns: 6rem 1fr 1fr;
		}
	}
	@media (max-width: 47.99rem) {
		.panels[data-tab='details'] .photo-panel {
			display: none;
		}
		.panels[data-tab='photo'] .details-panel {
			display: none;
		}
	}
</style>
