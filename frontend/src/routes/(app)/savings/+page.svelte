<script lang="ts">
	// Savings dashboard (P5-T4). Shows the household how much the app has saved
	// them: a cumulative net figure since first use, a per-month breakdown (gross
	// / accepted / net), and a table of archived lists with their outcomes.
	//
	// Like the optimise view this page is intrinsically online — it reads
	// server-aggregated `savings_events` via `GET /api/savings/summary` on mount.
	// SSR/prerender are disabled globally (src/routes/+layout.ts), so the on-mount
	// fetch only runs in the browser.
	//
	// All money is integer cents from the server; we never compute totals here.
	// Lists whose `chosen_basket_cost_cents` is null haven't had their actual
	// post-shop spend entered yet, so we show an "Enter actual spend" prompt that
	// links back to the list (the actual-spend entry lives on the list view).

	import { formatEuros } from '$lib/api/optimise';
	import { fetchSavingsSummary, type SavingsSummary } from '$lib/api/savings';

	type Status = 'loading' | 'ready' | 'error';

	let status = $state<Status>('loading');
	let summary = $state<SavingsSummary | null>(null);

	/**
	 * Load the summary. On any failure we surface a visible error and a retry
	 * rather than failing silently; the message stays generic because the
	 * endpoint's error bodies aren't a stable contract.
	 */
	async function load() {
		status = 'loading';
		try {
			summary = await fetchSavingsSummary();
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

	/** "2026-05" → "May 2026" for display; falls back to the raw value. */
	function monthLabel(month: string): string {
		const [year, mm] = month.split('-');
		const monthIndex = Number(mm) - 1;
		const names = [
			'January',
			'February',
			'March',
			'April',
			'May',
			'June',
			'July',
			'August',
			'September',
			'October',
			'November',
			'December'
		];
		const name = names[monthIndex];
		return name ? `${name} ${year}` : month;
	}

	/** "2026-05-03T10:00:00Z" → "3 May 2026"; falls back to the date part. */
	function dateLabel(iso: string): string {
		const d = new Date(iso);
		if (Number.isNaN(d.getTime())) return iso.slice(0, 10);
		return d.toLocaleDateString('en-IE', { day: 'numeric', month: 'short', year: 'numeric' });
	}
</script>

<svelte:head>
	<title>BuyWise — Savings</title>
</svelte:head>

<main class="savings-view">
	<header class="savings-header">
		<h1>Your savings</h1>
		<a class="back" href="/">Home</a>
	</header>

	{#if status === 'loading'}
		<p class="loading" role="status" aria-live="polite">Loading your savings…</p>
	{:else if status === 'error'}
		<div class="error" role="alert">
			<p>Couldn’t load your savings. Check your connection and try again.</p>
			<button type="button" class="retry" onclick={retry}>Try again</button>
		</div>
	{:else if summary}
		<section class="cumulative" aria-label="Cumulative saving">
			<p class="cumulative-label">Saved since you started</p>
			<p class="cumulative-amount">{formatEuros(summary.cumulative_net_cents)}</p>
			<p class="cumulative-note">Net of travel and subscription costs.</p>
		</section>

		<section class="monthly" aria-label="Monthly savings">
			<h2>By month</h2>
			{#if summary.monthly.length === 0}
				<p class="empty">No savings yet — archive a list with a recommendation to start tracking.</p>
			{:else}
				<ul class="month-list">
					{#each summary.monthly as m (m.month)}
						<li class="month-card">
							<div class="month-head">
								<span class="month-name">{monthLabel(m.month)}</span>
								<span class="month-lists">{m.lists} {m.lists === 1 ? 'list' : 'lists'}</span>
							</div>
							<dl class="month-figures">
								<div>
									<dt>Gross</dt>
									<dd>{formatEuros(m.gross_cents)}</dd>
								</div>
								<div>
									<dt>Accepted</dt>
									<dd>{formatEuros(m.accepted_cents)}</dd>
								</div>
								<div>
									<dt>Net</dt>
									<dd class="net">{formatEuros(m.net_cents)}</dd>
								</div>
							</dl>
						</li>
					{/each}
				</ul>
			{/if}
		</section>

		<section class="lists" aria-label="Archived lists">
			<h2>Archived lists</h2>
			{#if summary.lists.length === 0}
				<p class="empty">Your archived lists will appear here with their outcomes.</p>
			{:else}
				<ul class="outcome-list">
					{#each summary.lists as l (l.id)}
						<li class="outcome-card">
							<div class="outcome-head">
								<span class="outcome-name">{l.list_name}</span>
								<span class="outcome-date">{dateLabel(l.created_at)}</span>
							</div>
							<div class="outcome-body">
								<span class="outcome-gross">Gross saving {formatEuros(l.gross_saving_cents)}</span>
								{#if l.chosen_basket_cost_cents === null}
									<a class="enter-spend" href="/list/{l.list_id}">Enter actual spend</a>
								{:else}
									<span class="outcome-net"
										>Net {formatEuros(l.net_saving_cents ?? 0)}</span
									>
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
	.savings-view {
		max-width: 40rem;
		margin: 0 auto;
		padding: 1rem;
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}
	.savings-header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
	}
	.savings-header h1 {
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
		padding: 1rem;
		background: #fef2f2;
		border: 1px solid #fecaca;
		border-radius: 0.5rem;
		color: #991b1b;
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
		align-items: flex-start;
	}
	.retry {
		padding: 0.4rem 0.9rem;
		border: 1px solid #991b1b;
		background: white;
		color: #991b1b;
		border-radius: 0.4rem;
		cursor: pointer;
	}
	.cumulative {
		background: #f0fdfa;
		border: 1px solid #99f6e4;
		border-radius: 0.6rem;
		padding: 1.25rem;
		text-align: center;
	}
	.cumulative-label {
		margin: 0;
		font-size: 0.9rem;
		color: #0f766e;
	}
	.cumulative-amount {
		margin: 0.25rem 0;
		font-size: 2.2rem;
		font-weight: 700;
		color: #134e4a;
	}
	.cumulative-note {
		margin: 0;
		font-size: 0.8rem;
		color: #64748b;
	}
	h2 {
		font-size: 1.05rem;
		margin: 0 0 0.6rem;
	}
	.empty {
		color: #64748b;
		font-size: 0.9rem;
	}
	.month-list,
	.outcome-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.6rem;
	}
	.month-card,
	.outcome-card {
		border: 1px solid #e2e8f0;
		border-radius: 0.5rem;
		padding: 0.75rem 0.9rem;
	}
	.month-head,
	.outcome-head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		margin-bottom: 0.5rem;
	}
	.month-name,
	.outcome-name {
		font-weight: 600;
	}
	.month-lists,
	.outcome-date {
		font-size: 0.8rem;
		color: #64748b;
	}
	.month-figures {
		display: flex;
		gap: 1.25rem;
		margin: 0;
	}
	.month-figures div {
		display: flex;
		flex-direction: column;
	}
	.month-figures dt {
		font-size: 0.75rem;
		color: #64748b;
	}
	.month-figures dd {
		margin: 0;
		font-variant-numeric: tabular-nums;
	}
	.month-figures dd.net {
		font-weight: 600;
		color: #134e4a;
	}
	.outcome-body {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 0.75rem;
		font-size: 0.9rem;
	}
	.outcome-gross {
		color: #475569;
	}
	.outcome-net {
		font-weight: 600;
		color: #134e4a;
	}
	.enter-spend {
		font-size: 0.85rem;
		font-weight: 600;
		color: #0f766e;
		white-space: nowrap;
	}
</style>
