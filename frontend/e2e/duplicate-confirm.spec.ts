// Duplicate-warning "Add anyway" enqueue path — TRACKED GAP (issue #83).
//
// The rest of the sync-loop suite deliberately uses mutually-dissimilar grocery
// nouns (see fixtures.ts `uniqueName` / NOUNS) so every add stays on the fast,
// no-prompt path. That leaves the client-side duplicate detector's warning flow
// — DuplicateWarning -> "Add anyway" -> enqueue (AddItemInput.svelte) — without
// e2e coverage. This is an intentionally-scoped gap for PR #82; the placeholder
// below is `test.fixme` so it shows up as a pending todo rather than silently
// vanishing. See https://github.com/drkarl/buywise/issues/83 for the full ask.

import { test } from '@playwright/test';

test.describe('duplicate-warning "Add anyway" enqueue path', () => {
  // eslint-disable-next-line no-empty-pattern
  test.fixme('add a near-duplicate, confirm "Add anyway", and it syncs (issue #83)', async ({}) => {
    // Steps (see issue #83):
    //  1. Add an item, then add a near-duplicate that trips the detector.
    //  2. Assert the inline DuplicateWarning shows and the add is held (no
    //     optimistic row, no queued mutation yet).
    //  3. Click "Add anyway" → assert the add_item mutation enqueues and syncs
    //     to the second context like any other add.
  });
});
