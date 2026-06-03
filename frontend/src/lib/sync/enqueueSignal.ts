// Enqueue signal: lets the store notify a running sync engine of a new mutation.
//
// The P2-T3 store (`stores/list.ts`) is the single choke point through which
// every user action enqueues a `LocalMutation`. While online, edits would
// otherwise sit `pending` until the next bootstrap, manual refresh, or `online`
// event — defeating live sync. So after each successful enqueue the store calls
// `notifyMutationEnqueued(listId)`, and `startSync(listId)` subscribes (filtered
// to its own list) to flush the queue immediately through its existing in-flight
// sync chain. This keeps the store network-free and the drain serialised.

type Listener = (listId: string) => void;

const listeners = new Set<Listener>();

/**
 * Notifies all running sync engines that a mutation was enqueued for `listId`.
 * A no-op when nothing is subscribed (e.g. tests, or sync not started yet — the
 * mutation is still persisted and will drain on the next bootstrap/online pass).
 */
export function notifyMutationEnqueued(listId: string): void {
  for (const listener of listeners) listener(listId);
}

/**
 * Subscribes to enqueue notifications. Returns an unsubscribe function that
 * `startSync().stop()` must call on teardown so a torn-down engine stops
 * draining.
 */
export function onMutationEnqueued(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}
