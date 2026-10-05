<script lang="ts">
	// Always-visible sync/connectivity indicator (issue #113). Presentational: it
	// renders whatever {@link SyncStatus} the sync engine reports — synced,
	// syncing, offline (with the pending count), or error. The state is written
	// out as TEXT and an aria-live region, never signalled by colour alone.

	import { syncStatusLabel, type SyncStatus } from '$lib/sync/SyncStatus';

	interface Props {
		status: SyncStatus;
		ondismiss?: () => void;
	}

	let { status, ondismiss }: Props = $props();

	const label = $derived(syncStatusLabel(status));
	const icon = $derived(
		status.state === 'synced'
			? '✓'
			: status.state === 'syncing'
				? '⟳'
				: '⚠'
	);
</script>

<div
	class="sync-indicator"
	class:synced={status.state === 'synced'}
	class:syncing={status.state === 'syncing'}
	class:offline={status.state === 'offline'}
	class:error={status.state === 'error'}
	role="status"
	aria-live="polite"
	data-state={status.state}
	title={label}
>
	<span class="icon" aria-hidden="true">{icon}</span>
	<span class="text">{label}</span>
	{#if status.state === 'error' && ondismiss}
		<button type="button" class="dismiss" aria-label="Dismiss sync errors" onclick={ondismiss}>
			Dismiss
		</button>
	{/if}
</div>

<style>
	.sync-indicator {
		display: inline-flex;
		align-items: center;
		gap: 0.35rem;
		padding: 0.15rem 0.5rem;
		border: 1px solid transparent;
		border-radius: 999px;
		font-size: 0.8rem;
		line-height: 1.4;
		white-space: nowrap;
	}
	.icon {
		font-size: 0.85rem;
		line-height: 1;
	}
	.synced {
		color: #0f766e;
		background: #f0fdfa;
		border-color: #99f6e4;
	}
	.syncing {
		color: #1d4ed8;
		background: #eff6ff;
		border-color: #bfdbfe;
	}
	.offline {
		color: #92400e;
		background: #fffbeb;
		border-color: #fde68a;
	}
	.error {
		color: #991b1b;
		background: #fef2f2;
		border-color: #fecaca;
	}
	.syncing .icon {
		animation: sync-spin 1.4s linear infinite;
	}
	.dismiss {
		padding: 0;
		border: 0;
		background: transparent;
		color: inherit;
		font: inherit;
		text-decoration: underline;
		cursor: pointer;
	}
	@keyframes sync-spin {
		from {
			transform: rotate(0deg);
		}
		to {
			transform: rotate(360deg);
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.syncing .icon {
			animation: none;
		}
	}
	@media (max-width: 30rem) {
		.text {
			/* Keep the longest label ("Offline — 2 changes pending") from pushing
			   the header count off a phone screen; the status text wraps under the
			   icon instead. */
			white-space: normal;
		}
	}
</style>
