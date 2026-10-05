import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import SyncIndicator from './SyncIndicator.svelte';
import { deriveSyncStatus, type SyncStatusInput } from '$lib/sync/SyncStatus';

function renderIndicator(input: SyncStatusInput) {
  return render(SyncIndicator, { props: { status: deriveSyncStatus(input) } });
}

describe('SyncIndicator', () => {
  it('shows "Synced" when online and settled', () => {
    renderIndicator({ online: true, connected: true });
    const el = screen.getByRole('status');
    expect(el.textContent).toContain('Synced');
    expect(el.getAttribute('data-state')).toBe('synced');
  });

  it('shows the pending count while syncing', () => {
    renderIndicator({ online: true, connected: true, pending: 2, syncing: true });
    const el = screen.getByRole('status');
    expect(el.textContent).toContain('Syncing 2 changes…');
    expect(el.getAttribute('data-state')).toBe('syncing');
  });

  it('shows a stalled queue as offline instead of syncing', () => {
    renderIndicator({ online: true, connected: true, pending: 2 });
    const el = screen.getByRole('status');
    expect(el.textContent).toContain('Offline — 2 changes pending');
    expect(el.getAttribute('data-state')).toBe('offline');
  });

  it('shows offline with the pending count', () => {
    renderIndicator({ online: false, connected: false, pending: 1 });
    const el = screen.getByRole('status');
    expect(el.textContent).toContain('Offline — 1 change pending');
    expect(el.getAttribute('data-state')).toBe('offline');
  });

  it('shows the error state when a change was rejected', () => {
    renderIndicator({ online: true, connected: true, failed: 1 });
    const el = screen.getByRole('status');
    expect(el.textContent).toContain('Sync error — 1 change couldn’t be saved');
    expect(el.getAttribute('data-state')).toBe('error');
  });

  it('offers one action to clear persisted sync errors', async () => {
    const ondismiss = vi.fn();
    render(SyncIndicator, {
      props: { status: deriveSyncStatus({ failed: 2 }), ondismiss }
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Dismiss sync errors' }));
    expect(ondismiss).toHaveBeenCalledOnce();
  });

  it('exposes the state as a polite live region, as text and not colour alone', () => {
    renderIndicator({ online: false, connected: false, pending: 3 });
    const el = screen.getByRole('status');
    expect(el.getAttribute('aria-live')).toBe('polite');
    // The state is present as readable text, not signalled by colour alone.
    expect(el.textContent).toContain('Offline');
  });
});
