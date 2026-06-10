// Shared helpers for the P2-T4 sync-loop e2e suite (issue #63).
//
// Two browser CONTEXTS stand in for two devices on the same household list.
// Both log in as the one seeded user (see e2e/seed.sh); their edits therefore
// sync to each other through the real backend exactly as two phones would. All
// state assertions read the SPA's own Dexie/IndexedDB inside the page so we are
// testing the real local store the sync engine writes, not a UI re-render proxy.

import { type BrowserContext, type Page, expect } from '@playwright/test';

/** Fixed identifiers, mirrored from e2e/seed.sh. */
export const SEED = {
  email: process.env.E2E_EMAIL ?? 'e2e@buywise.test',
  password: process.env.E2E_PASSWORD ?? 'e2e-password-123',
  listId: process.env.E2E_LIST_ID ?? 'list-e2e'
} as const;

/** The in-app path for the seeded list. */
export const listPath = `/list/${SEED.listId}`;

// Distinct grocery nouns for item names. The AddItemInput runs a real
// client-side duplicate check (matching/duplicate.ts) against the active list
// and HOLDS an add behind an inline warning when the new text looks like an
// existing item — so item names that share tokens (e.g. "Race 123-1" vs
// "Race 123-2") trip the detector and never actually add. Using mutually
// dissimilar real nouns keeps every add on the fast, no-prompt path. The
// per-call suffix (see uniqueName) must NOT be a bare number: the duplicate
// check runs `parseQuantityAndUnit`, which strips a trailing bare number as a
// COUNT token — so "Quinoa 17" would normalise to "quinoa" and collide with a
// later "Quinoa 23". We therefore make the suffix an ALPHA token so it survives
// normalisation and keeps every name genuinely distinct.
const NOUNS = [
  'Quinoa', 'Lentils', 'Paprika', 'Walnuts', 'Cinnamon', 'Oregano', 'Cashews',
  'Turmeric', 'Couscous', 'Chickpeas', 'Almonds', 'Saffron', 'Pistachios',
  'Cardamom', 'Hazelnuts', 'Rosemary', 'Mango', 'Papaya', 'Avocado', 'Spinach'
];
let nounCursor = 0;
let nameSeq = 0;

/**
 * Returns a fresh, duplicate-safe item name: a distinct grocery noun plus an
 * ALPHA suffix. Successive calls draw different nouns so no two live items look
 * like duplicates to the client-side detector, and the suffix keeps names unique
 * across specs sharing the one seeded list. The suffix is base-36 letters (no
 * bare trailing number) so `parseQuantityAndUnit` does not strip it as a count
 * token and collapse distinct names together (see the NOUNS comment above).
 */
export function uniqueName(): string {
  const noun = NOUNS[nounCursor % NOUNS.length];
  nounCursor += 1;
  nameSeq += 1;
  // Base-36 of a timestamp + counter, with digits mapped to letters so the whole
  // token is alphabetic and survives count/size normalisation intact.
  const raw = `${Date.now().toString(36)}${nameSeq.toString(36)}`;
  const suffix = raw.replace(/[0-9]/g, (d) => String.fromCharCode(103 + Number(d))); // 0→g … 9→p
  return `${noun} ${suffix}`;
}

/**
 * Logs the given context in via the real `POST /api/auth/login`. The backend
 * sets the HttpOnly `session` and JS-readable `csrf` cookies on the response,
 * scoped to the context — so every subsequent page in this context is
 * authenticated and can read the csrf token for mutations. We log in at the API
 * (not a UI form) because the app has no login screen yet (auth is cookie-only).
 */
export async function login(context: BrowserContext): Promise<void> {
  const res = await context.request.post('/api/auth/login', {
    data: { email: SEED.email, password: SEED.password }
  });
  expect(res.ok(), `login failed: ${res.status()} ${await res.text()}`).toBeTruthy();
}

/**
 * Opens the seeded list view in a fresh, authenticated page for `context` and
 * waits for the sync engine to have bootstrapped (the list view is mounted and
 * the initial snapshot/drain has settled). Returns the page.
 */
export async function openList(context: BrowserContext): Promise<Page> {
  await login(context);
  const page = await context.newPage();
  await page.goto(listPath);
  // The list heading renders once the SPA has hydrated this route.
  await expect(page.getByRole('heading', { name: 'Shopping list' })).toBeVisible();
  return page;
}

/** Adds an item through the real UI (enqueues an `add_item` mutation). */
export async function addItem(page: Page, text: string): Promise<void> {
  const input = page.getByRole('textbox', { name: 'Add an item' });
  await input.fill(text);
  await page.getByRole('button', { name: 'Add item' }).click();
  await expect(input).toHaveValue('');
}

/** The active-list row for an item, located by its accessible group label. */
export function activeRow(page: Page, name: string) {
  return page.getByRole('group', { name, exact: true });
}

// The check-off and skip buttons sit INSIDE the row's `.swipe` gesture surface,
// whose `onpointerdown` calls `setPointerCapture`. A real Playwright `.click()`
// does NOT fire the button's handler: pointer capture redirects the `pointerup`
// to the `.swipe` div, so Chromium dispatches the synthesized `click` at the div
// (the captured target), not the inner button — the item silently stays active.
//
// This is a CONFIRMED PRODUCT BUG, reproduced in this harness and tracked in
// https://github.com/drkarl/buywise/issues/84 (a real tap is absorbed by the
// swipe layer). Until that is fixed we dispatch a `click` straight on the button
// to invoke its own handler — the same effect the *fixed* component will give a
// tap. Re-evaluate / switch to a real `.click()` once #84 lands.
// (The Add button is outside the swipe surface, so it uses a normal `.click()`.)

/** Checks an active item off (enqueues `mark_bought`). See issue #84 re: dispatch. */
export async function markBought(page: Page, name: string): Promise<void> {
  await page.getByRole('button', { name: `Check off ${name}` }).dispatchEvent('click');
}

/** Skips an active item (enqueues `mark_skipped`). See issue #84 re: dispatch. */
export async function markSkipped(page: Page, name: string): Promise<void> {
  await page.getByRole('button', { name: `Skip ${name}` }).dispatchEvent('click');
}

/**
 * Deletes an active item via its long-press context menu. We dispatch a real
 * `pointerdown` on the row's `.swipe` group (driving the menu via absolute mouse
 * coordinates is flaky: the row can shift and the hold timing races layout). The
 * component's handler starts a 500ms timer that opens the menu; we wait on the
 * revealed Delete button (no fixed sleep) and then click it — Delete is a SIBLING
 * of the swipe surface, so its click is not affected by the issue #84 capture
 * retargeting and a real `.click()` works here.
 */
export async function deleteItem(page: Page, name: string): Promise<void> {
  const row = activeRow(page, name);
  await row.scrollIntoViewIfNeeded();
  // Fire pointerdown with a real clientX/pointerId so the row's long-press timer
  // arms exactly as it does for a user. setPointerCapture is guarded in the
  // component, so a synthetic pointerId is safe.
  await row.dispatchEvent('pointerdown', { clientX: 10, clientY: 10, pointerId: 1, button: 0 });
  // Wait on the menu's Delete button appearing (driven by the 500ms LONG_PRESS_MS
  // timer) rather than sleeping a fixed amount coupled to that constant.
  const del = page.getByRole('button', { name: 'Delete', exact: true });
  await del.waitFor({ state: 'visible', timeout: 5_000 });
  await del.click();
}

/** A Dexie list-entry as projected for assertions (subset of LocalListEntry). */
export interface EntrySnapshot {
  id: string;
  list_id: string;
  display_name: string;
  status: string;
  quantity: number;
  last_updated_revision: number;
}

/**
 * Reads every `list_entries` row for `listId` straight out of the page's
 * IndexedDB (the `buywise` Dexie database), sorted by display_name then id for
 * stable comparison. This is the ground truth the sync engine writes — we assert
 * over it rather than the rendered DOM so non-active rows (bought/skipped/
 * deleted tombstones) are visible to the full-convergence check.
 */
export async function dexieEntries(page: Page, listId: string): Promise<EntrySnapshot[]> {
  return page.evaluate(async (lid) => {
    function openDb(): Promise<IDBDatabase> {
      return new Promise((resolve, reject) => {
        const req = indexedDB.open('buywise');
        req.onsuccess = () => resolve(req.result);
        req.onerror = () => reject(req.error);
      });
    }
    const db = await openDb();
    const rows: Record<string, unknown>[] = await new Promise((resolve, reject) => {
      const tx = db.transaction('list_entries', 'readonly');
      const store = tx.objectStore('list_entries');
      const out: Record<string, unknown>[] = [];
      const cursorReq = store.openCursor();
      cursorReq.onsuccess = () => {
        const cursor = cursorReq.result;
        if (cursor) {
          out.push(cursor.value as Record<string, unknown>);
          cursor.continue();
        } else {
          resolve(out);
        }
      };
      cursorReq.onerror = () => reject(cursorReq.error);
    });
    db.close();
    return rows
      .filter((r) => r.list_id === lid)
      .map((r) => ({
        id: String(r.id),
        list_id: String(r.list_id),
        display_name: String(r.display_name),
        status: String(r.status),
        quantity: Number(r.quantity),
        last_updated_revision: Number(r.last_updated_revision)
      }))
      .sort((a, b) =>
        a.display_name === b.display_name
          ? a.id.localeCompare(b.id)
          : a.display_name.localeCompare(b.display_name)
      );
  }, listId);
}

/** Reads the local `list_meta.current_revision` for a list from the page's Dexie. */
export async function dexieRevision(page: Page, listId: string): Promise<number> {
  return page.evaluate(async (lid) => {
    function openDb(): Promise<IDBDatabase> {
      return new Promise((resolve, reject) => {
        const req = indexedDB.open('buywise');
        req.onsuccess = () => resolve(req.result);
        req.onerror = () => reject(req.error);
      });
    }
    const db = await openDb();
    const meta: Record<string, unknown> | undefined = await new Promise((resolve, reject) => {
      const tx = db.transaction('list_meta', 'readonly');
      const getReq = tx.objectStore('list_meta').get(lid);
      getReq.onsuccess = () => resolve(getReq.result as Record<string, unknown> | undefined);
      getReq.onerror = () => reject(getReq.error);
    });
    db.close();
    return meta ? Number(meta.current_revision) : 0;
  }, listId);
}

/**
 * The authoritative server snapshot via `GET /api/list/:id`, projected to the
 * same `EntrySnapshot` shape and ordering as {@link dexieEntries} so the two can
 * be compared directly. As of issue #62 this carries the COMPLETE entry set
 * (every status incl. bought/skipped/deleted), enabling true convergence checks.
 */
export async function serverEntries(
  context: BrowserContext,
  listId: string
): Promise<EntrySnapshot[]> {
  const res = await context.request.get(`/api/list/${listId}`);
  expect(res.ok(), `snapshot fetch failed: ${res.status()}`).toBeTruthy();
  const body = (await res.json()) as {
    entries: Array<{
      id: string;
      list_id: string;
      display_name: string;
      status: string;
      quantity: number;
      last_updated_revision: number;
    }>;
  };
  return body.entries
    .map((e) => ({
      id: e.id,
      list_id: e.list_id,
      display_name: e.display_name,
      status: e.status,
      quantity: e.quantity,
      last_updated_revision: e.last_updated_revision
    }))
    .sort((a, b) =>
      a.display_name === b.display_name
        ? a.id.localeCompare(b.id)
        : a.display_name.localeCompare(b.display_name)
    );
}

/**
 * Polls until the page's mutation queue for `listId` has no `pending` or `sent`
 * rows — i.e. every locally-enqueued edit has reached the server (confirmed) or
 * been terminally rejected (failed). Use after a reconnect to wait for the drain
 * to fully settle before asserting convergence: an offline mutation first burns
 * its real retry/backoff budget against the dead network, so the reconnect drain
 * timing is otherwise nondeterministic.
 */
export async function waitForDrained(page: Page, listId: string): Promise<void> {
  await expect
    .poll(
      async () =>
        page.evaluate(async (lid) => {
          function openDb(): Promise<IDBDatabase> {
            return new Promise((resolve, reject) => {
              const req = indexedDB.open('buywise');
              req.onsuccess = () => resolve(req.result);
              req.onerror = () => reject(req.error);
            });
          }
          const db = await openDb();
          const unsettled: number = await new Promise((resolve, reject) => {
            const tx = db.transaction('mutation_queue', 'readonly');
            const store = tx.objectStore('mutation_queue');
            let count = 0;
            const cursorReq = store.openCursor();
            cursorReq.onsuccess = () => {
              const cursor = cursorReq.result;
              if (cursor) {
                const v = cursor.value as { list_id: string; status: string };
                if (v.list_id === lid && (v.status === 'pending' || v.status === 'sent')) {
                  count += 1;
                }
                cursor.continue();
              } else {
                resolve(count);
              }
            };
            cursorReq.onerror = () => reject(cursorReq.error);
          });
          db.close();
          return unsettled;
        }, listId),
      { message: 'mutation queue never fully drained', timeout: 30_000 }
    )
    .toBe(0);
}

/**
 * Counts `add_item` mutations for `name` currently sitting in the page's local
 * `mutation_queue` (any status), matched on the mutation's `payload.display_name`
 * — the text the add was enqueued with (see stores/list.ts `addItem`). Used by
 * the duplicate-warning spec to prove an add is HELD (0 queued while the inline
 * warning is up) versus ENQUEUED (1 queued after "Add anyway"), reading the real
 * queue the sync engine drains rather than any UI proxy.
 */
export async function queuedAddCount(
  page: Page,
  listId: string,
  name: string
): Promise<number> {
  return page.evaluate(
    async ({ lid, wanted }) => {
      function openDb(): Promise<IDBDatabase> {
        return new Promise((resolve, reject) => {
          const req = indexedDB.open('buywise');
          req.onsuccess = () => resolve(req.result);
          req.onerror = () => reject(req.error);
        });
      }
      const db = await openDb();
      const count: number = await new Promise((resolve, reject) => {
        const tx = db.transaction('mutation_queue', 'readonly');
        const store = tx.objectStore('mutation_queue');
        let matches = 0;
        const cursorReq = store.openCursor();
        cursorReq.onsuccess = () => {
          const cursor = cursorReq.result;
          if (cursor) {
            const v = cursor.value as {
              list_id: string;
              operation_type: string;
              payload?: { display_name?: string };
            };
            if (
              v.list_id === lid &&
              v.operation_type === 'add_item' &&
              v.payload?.display_name === wanted
            ) {
              matches += 1;
            }
            cursor.continue();
          } else {
            resolve(matches);
          }
        };
        cursorReq.onerror = () => reject(cursorReq.error);
      });
      db.close();
      return count;
    },
    { lid: listId, wanted: name }
  );
}

/**
 * Polls until the page's Dexie has an entry named `name` in `status`. Used to
 * wait for an SSE-delivered or drained edit to land locally without sleeping.
 */
export async function expectDexieEntry(
  page: Page,
  listId: string,
  name: string,
  status: string
): Promise<void> {
  await expect
    .poll(
      async () => {
        const entries = await dexieEntries(page, listId);
        return entries.some((e) => e.display_name === name && e.status === status);
      },
      { message: `Dexie never showed "${name}" in status "${status}"`, timeout: 15_000 }
    )
    .toBeTruthy();
}
