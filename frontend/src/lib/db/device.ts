// Stable per-device identifier used as `device_id` in every LocalMutation.
//
// Per the implementation plan: generate a UUID v4 on first load, persist it in
// localStorage, and reuse it thereafter. The value is intentionally per-browser
// (not per-user) so that mutations queued offline can be attributed to the
// originating device once the sync loop (P2-T4) drains the queue.

const STORAGE_KEY = 'buywise_device_id';

let cached: string | null = null;

/**
 * Returns this device's stable UUID v4, creating and persisting one on first
 * use. Falls back to an in-memory id if localStorage is unavailable (e.g. tests
 * or privacy modes) so callers always receive a usable identifier.
 */
export function getDeviceId(): string {
  if (cached) return cached;

  try {
    const existing = localStorage.getItem(STORAGE_KEY);
    if (existing) {
      cached = existing;
      return existing;
    }
    const generated = crypto.randomUUID();
    localStorage.setItem(STORAGE_KEY, generated);
    cached = generated;
    return generated;
  } catch {
    // localStorage can throw (disabled cookies, SSR, sandboxed iframe). Keep a
    // process-lifetime id so mutations remain internally consistent.
    cached ??= crypto.randomUUID();
    return cached;
  }
}
