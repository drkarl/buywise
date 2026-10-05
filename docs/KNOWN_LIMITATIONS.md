# Known Limitations

Shipped features record their known limitations here, in the repository, instead of only in the pull
request that introduced them — merged PR descriptions are lost and are not a place operators search.
Add one section per feature when a ship carries a known limitation. Each entry gives the limitation,
what happens, why it was accepted, a workaround or none, and the follow-up that removes it (a tracked
issue, a backlog id, or an explicit open question). A limitation that must hold a release is marked
**RELEASE BLOCKER** at the top of its section.

_Last reviewed: 2026-10-05._

## Per-store trip view

Splits the chosen optimiser plan into one shopping list per store (issue #104, delivered by
[PR #125](https://github.com/drkarl/buywise/pull/125)). Design: `docs/design.md` §10.

> PR #125 is not yet merged into this branch's base, so this section records the accepted gaps from
> that change; it could not be confirmed against the checked-out code.

### 1. Store-scoped shopping mode has no stale-plan warning, and the chosen trip is per-device

**What happens.** Shopping mode for one store renders only that store's allocated entries. It shows
no stale-plan warning, so an entry added to the list after the plan was chosen — or an item the plan
left unallocated — is neither visible nor check-off-able there. The only affordance is the "N items
not in a store list" link back to the whole trip; the stale-plan banner is rendered only on the trip
page.

The chosen allocation is persisted per device in `localStorage` (issue #104), not in the database.
The other household device does not see the choice until it picks a plan itself, and the trip's
Estimated totals keep the prices captured when the plan was chosen until the list is re-optimised.
There is also no per-line manual reassignment in this volume — re-selecting a plan replaces the
allocation.

**Why accepted.** The per-store lists are views over the shared entries and every check-off still
flows through the offline queue, so devices converge on the data even though each holds its own plan
snapshot. Adding a second stale banner to shopping mode and making the chosen plan server state were
beyond the change's stated scope.

**Workaround.** Open `/list/<id>/trip` for the whole trip — its stale banner links back to the
optimiser, and the unallocated section can be checked off there. Re-optimise and re-choose on each
device; the shared entries and their statuses stay in sync.

**Follow-up.** Issue #104 follow-up: show the stale-plan warning (or the not-in-a-store-list notice)
inside store-scoped shopping mode, and decide whether the chosen plan becomes shared server state.
Add per-line reassignment if the workflow needs it.

## Optimiser CLI and core

### 2. The CLI prints a zero two-store saving when the plans are not comparable

**What happens.** `buywise-cli optimise` prints `two-store saving vs best single: €0.00 …` when the
recommended plan and the best single store price different item sets. `Recommendation.two_store_saving_cents`
(`crates/core/src/optimiser.rs:339`) is a non-optional `Cents` that is set to `0` when the two item
sets differ (`optimiser.rs:1272`, `:1298-1302`, `:1326`, `:1338`), and `print_recommendation`
(`crates/cli/src/commands/optimise.rs:170-183`) renders that zero as a euro amount. It reads as a
measured zero saving rather than "not comparable". The optimise screen avoids this by suppressing the
saving line when the item sets differ (`frontend/src/lib/components/PlanCard.svelte`, `savingCents`).

**Why accepted.** The wire field predates the coverage guard; reporting no saving is the safe default,
and the CLI is a Phase-0 validation tool rather than the household UI.

**Workaround.** Compare the plans' item sets / `missing_count` directly; do not read the printed
`€0.00` as a real zero.

**Follow-up.** Make the field optional (or add a comparability flag) so the CLI can print
"not comparable" instead of `€0.00`.

### 3. `nearby_applied` is set but never read

**What happens.** `RecommendationDecision::nearby_applied` (`crates/core/src/optimiser.rs:1187`) is
assigned by `decide_recommendation` (`:1241`, `:1275`, `:1305`, plus the `false` defaults at `:1331`,
`:1343`, `:1350`) but is never read. The reason generator derives the wording from
`threshold_cents` instead (`optimiser.rs:1498`), so the flag is dead decision-context state.

**Why accepted.** It was added while introducing the nearby-store bar (issue #98) and left behind
when the renderer used the threshold directly.

**Workaround.** None needed — dead code only.

**Follow-up.** Remove the field and its assignments once confirmed unused.

## Sync indicator

### 4. A pending item can briefly show un-ticked after Dismiss

**What happens.** After **Dismiss** on a sync error, `dismissFailures` (`frontend/src/lib/sync/index.ts:268`)
re-fetches the list snapshot and applies it with `reconcile: true`. That apply exempts entries with a
pending/in-flight mutation from its `staleActive` cleanup (`frontend/src/lib/sync/SseClient.ts:259-262`),
but the following `bulkPut(entries)` (`:263`) upserts the server's rows unconditionally. A *pending
non-add* mutation (mark bought / skip) is therefore not protected there: the item it optimistically
changed reverts to the server's older state — a pending item can briefly show un-ticked — until the
next drain confirms it.

**Why accepted.** The reconcile exists to repair terminal 4xx state; the transient revert is
self-healing on the next drain.

**Workaround.** None — wait for the next drain, which restores the change.

**Follow-up.** Guard the reconcile `bulkPut` so optimistic rows backing pending/sent mutations are
preserved, as pending adds already are.

## Store settings

### 5. The trip toggle unmounts during re-optimise

**What happens.** The optimise screen's "Going anywhere else today?" trip toggles live inside the
`{:else if result}` branch (`frontend/src/routes/(app)/list/[id]/optimise/+page.svelte:267-300`).
Clicking a toggle calls `toggleTrip` (`:86`), which re-runs `runOptimise` and sets `status = 'loading'`;
the whole result subtree — trip toggles included — unmounts and is replaced by the loading message
until the new plan arrives. The toggle flickers out and back and drops keyboard focus.

**Why accepted.** Re-optimising needs the loading state and the control is small; keeping it mounted
across a re-optimise was out of scope for the store-settings change (issue #98).

**Workaround.** None.

**Follow-up.** Keep the trip controls mounted across a re-optimise (render them outside the loading
branch, or scope the loading state to the plans region).
