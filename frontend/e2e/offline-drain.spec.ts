// Offline → reconnect drain (issue #63 scope: "Offline → reconnect drain").
//
// Context A goes offline, makes a batch of edits across all four mutation kinds
// (add_item, mark_bought, mark_skipped, delete_item), then reconnects. The
// queued mutations must drain IN CREATION ORDER (SyncQueue replays by ascending
// id), and A must converge with the authoritative server state observed in
// context B. We exercise dependent ordering on purpose: an add followed by a
// mark_bought / delete on the SAME entry only succeeds if the add drains first.

import { test, expect } from '@playwright/test';
import {
  SEED,
  openList,
  addItem,
  markBought,
  markSkipped,
  deleteItem,
  expectDexieEntry,
  dexieEntries,
  serverEntries,
  activeRow,
  uniqueName,
  waitForDrained
} from './fixtures';

test.describe('offline → reconnect drain', () => {
  test('all four mutation kinds queue offline and drain in order on reconnect', async ({
    browser
  }) => {
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    try {
      const a = await openList(ctxA);
      const b = await openList(ctxB);

      // Observe the drain SEQUENCE directly: a pass-through route on A records
      // every POST /api/mutations in the order the client sends it (the drain
      // order), capturing operation_type + entity_id + the idempotency key. We
      // assert this observed order below — convergence alone could pass on an
      // out-of-order drain, but the issue criterion is explicitly "drain IN
      // ORDER". A single mutation may be POSTed more than once (the offline retry
      // budget burns attempts against the dead network, and each is an idempotent
      // replay of the SAME client_mutation_id), so we dedupe by that key and
      // reason over the order of each mutation's FIRST send.
      const sends: Array<{ op: string; entityId: string; cmid: string }> = [];
      await a.route('**/api/mutations', async (route) => {
        const body = route.request().postDataJSON() as {
          operation_type: string;
          entity_id: string;
          client_mutation_id: string;
        };
        sends.push({
          op: body.operation_type,
          entityId: body.entity_id,
          cmid: body.client_mutation_id
        });
        await route.continue();
      });

      // Distinct nouns so the client-side duplicate check never holds an add.
      const toBuy = uniqueName(); // add, then mark_bought (dependent on the add)
      const toSkip = uniqueName(); // add, then mark_skipped
      const toDelete = uniqueName(); // add, then delete (dependent on the add)
      const toKeep = uniqueName(); // add, stays active

      // A goes offline. Every edit below is optimistic-only and queues in Dexie.
      await ctxA.setOffline(true);

      await addItem(a, toBuy);
      await addItem(a, toSkip);
      await addItem(a, toDelete);
      await addItem(a, toKeep);
      // Optimistic rows are visible while offline (no network involved).
      await expect(activeRow(a, toKeep)).toBeVisible();

      await markBought(a, toBuy);
      await markSkipped(a, toSkip);
      await deleteItem(a, toDelete);

      // Nothing has reached the server yet: B (online) must NOT see these.
      const serverBeforeReconnect = await serverEntries(ctxB, SEED.listId);
      for (const name of [toBuy, toSkip, toDelete, toKeep]) {
        expect(serverBeforeReconnect.some((e) => e.display_name === name)).toBeFalsy();
      }

      // Reconnect → the `online` event drives the drain (in id/creation order).
      await ctxA.setOffline(false);

      // Wait for A's queue to fully drain before asserting convergence: each
      // offline mutation first burns its retry/backoff budget against the dead
      // network, so the reconnect drain settles asynchronously.
      await waitForDrained(a, SEED.listId);

      // B observes the drained, converged server truth via SSE: kept item active,
      // bought/skipped in basket, deleted as a tombstone.
      await expectDexieEntry(b, SEED.listId, toKeep, 'active');
      await expectDexieEntry(b, SEED.listId, toBuy, 'bought');
      await expectDexieEntry(b, SEED.listId, toSkip, 'skipped');
      await expectDexieEntry(b, SEED.listId, toDelete, 'deleted');

      // A's own local Dexie reflects the same statuses after the drain confirms.
      await expectDexieEntry(a, SEED.listId, toKeep, 'active');
      await expectDexieEntry(a, SEED.listId, toBuy, 'bought');
      await expectDexieEntry(a, SEED.listId, toSkip, 'skipped');
      await expectDexieEntry(a, SEED.listId, toDelete, 'deleted');

      // Ordering proof (DIRECT, EXACT): collapse retries by keeping each
      // mutation's FIRST send (by client_mutation_id), preserving order. The
      // deduped stream is the true drain sequence the SyncQueue replayed.
      const firstSends: typeof sends = [];
      const seen = new Set<string>();
      for (const s of sends) {
        if (!seen.has(s.cmid)) {
          seen.add(s.cmid);
          firstSends.push(s);
        }
      }

      // Resolve each item's entity id from A's Dexie so we can assert the drain
      // sequence by (op → the SPECIFIC entity), not by op alone.
      const localById = await dexieEntries(a, SEED.listId);
      const idOf = (name: string) => {
        const row = localById.find((e) => e.display_name === name);
        if (!row) throw new Error(`no local entry for "${name}"`);
        return row.id;
      };

      // The mutations were created in EXACTLY this order (four adds in creation
      // order, then the dependent ops), and SyncQueue replays by ascending id, so
      // the observed drain sequence must equal it field-for-field: same ops, same
      // target entities, same order. A regression that reorders the adds among
      // themselves, or moves a dependent op before/among the adds, fails here —
      // not just the prefix/per-pair spot checks the looser version allowed.
      const expectedSequence = [
        { op: 'add_item', entityId: idOf(toBuy) },
        { op: 'add_item', entityId: idOf(toSkip) },
        { op: 'add_item', entityId: idOf(toDelete) },
        { op: 'add_item', entityId: idOf(toKeep) },
        { op: 'mark_bought', entityId: idOf(toBuy) },
        { op: 'mark_skipped', entityId: idOf(toSkip) },
        { op: 'delete_item', entityId: idOf(toDelete) }
      ];
      expect(firstSends.map((s) => ({ op: s.op, entityId: s.entityId }))).toEqual(
        expectedSequence
      );

      // Convergence proof: confirm the server's final state for these four items
      // matches A's local state.
      const serverFinal = (await serverEntries(ctxB, SEED.listId)).filter((e) =>
        [toBuy, toSkip, toDelete, toKeep].includes(e.display_name)
      );
      const localA = (await dexieEntries(a, SEED.listId)).filter((e) =>
        [toBuy, toSkip, toDelete, toKeep].includes(e.display_name)
      );
      const statusByName = (rows: typeof serverFinal) =>
        Object.fromEntries(rows.map((r) => [r.display_name, r.status]));
      expect(statusByName(localA)).toEqual(statusByName(serverFinal));
      expect(statusByName(serverFinal)).toEqual({
        [toKeep]: 'active',
        [toBuy]: 'bought',
        [toSkip]: 'skipped',
        [toDelete]: 'deleted'
      });
    } finally {
      await ctxA.close();
      await ctxB.close();
    }
  });
});
