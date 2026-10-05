# Known Limitations

Shipped features record their known limitations here, in the repository, instead of only in the pull
request that introduced them — merged PR descriptions are lost and are not a place operators search.
Add one section per feature when a ship carries a known limitation. Each entry gives the limitation,
what happens, why it was accepted, a workaround or none, and the follow-up that removes it (a tracked
issue, a backlog id, or an explicit open question). A limitation that must hold a release is marked
**RELEASE BLOCKER** at the top of its section.

_Last reviewed: 2026-10-05._

## Per-store trip view

Splits the chosen optimiser plan into one shopping list per store — the trip view, the store-scoped
shopping mode, and the allocation backing them (issue #104, merged in
[PR #125](https://github.com/drkarl/buywise/pull/125)). Design: `docs/design.md` §10.

### 1. Store-scoped shopping mode has no stale-plan warning, and the chosen trip is a per-device cache

**What happens.** Shopping mode scoped to one store
(`frontend/src/routes/(app)/list/[id]/shopping/+page.svelte`) renders only that store's allocated
entries (`storeEntryIds`, `:70-75`). It reads the chosen trip but never calls `isTripStale`, so it
shows no stale-plan warning; an entry added after the plan was chosen, and an item the plan left
unallocated, are neither rendered nor check-off-able there. The only affordance is the
"N item(s) not in a store list — View whole trip" link (`:143-151`), whose count is the active
entries that are unallocated or absent from `allocation.entryIds` (`:75-84`); the stale-plan banner
lives only on the trip page (`frontend/src/routes/(app)/list/[id]/trip/+page.svelte:153-158`). An
entry added after choosing is counted by that link but has no per-item view in either the trip or
store-scoped shopping view — its "Not allocated to a store" section (`trip/+page.svelte:183-212`)
carries only the items the plan itself left unallocated. The entry remains available on the plain
list page (`frontend/src/routes/(app)/list/[id]/+page.svelte:223-249`), where it can be checked off.

The chosen allocation is a client-side cache in `localStorage` under `buywise:trip:<listId>`
(`frontend/src/lib/shopping/chosenTrip.ts:33`, `:57`), not server state. It is therefore per device:
another household device does not see the choice until it picks a plan itself — its trip page shows
"No plan selected yet" (`trip/+page.svelte:148-151`) and its shopping mode falls back to all five
stores (`shopping/+page.svelte:38-42`). The cache is never invalidated by app code
(`clearChosenTrip`, `chosenTrip.ts:64`, is exported but only tests call it), and staleness is
id-only (`chosenTrip.ts:82-86`): a quantity edit through `setQuantity`
(`frontend/src/lib/stores/list.ts:304`) keeps the entry's id, so the trip still reports the chosen
product and the frozen `Estimated` per-store totals (`trip/+page.svelte:164`) as current. The
allocation only changes by re-selecting a plan on the optimise page, which overwrites the cache
(`optimise/+page.svelte:182-201`); there is no per-line manual reassignment.

**Why accepted.** The per-store lists are views over the shared list entries and every check-off
still flows through the normal offline queue, so devices converge on the entries even though each
holds its own plan snapshot. `localStorage` avoids a Dexie schema migration for a small, per-device
snapshot, and design §10 records the per-device cache, the replace-by-reselecting workflow, and the
frozen-price snapshot as deliberate volume-one scope; a second stale banner inside shopping mode was
out of scope.

**Workaround.** Open `/list/<id>` to view and check off an entry added after choosing a plan. Open
`/list/<id>/trip` for the whole trip — its stale banner links back to the optimiser. Re-optimise and
re-choose on each device; the shared entries and their statuses stay in sync.

**Follow-up.** Issue #104 follow-up: show the stale-plan warning (or the not-in-a-store-list notice)
inside store-scoped shopping mode, extend the stale check to quantity and other plan-affecting
edits, and decide whether the chosen plan becomes shared server state. Add per-line reassignment if
the workflow needs it.

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

## Household product preferences

### 5. Only the `brand` policy changes behaviour; `own_label`, `exact` and `ask` are stored but inert

**What happens.** The per-item preferences screen (`/items/{id}/preferences`, issue #102) offers all
six `ConstraintPolicy` values. The optimiser, however, special-cases only `brand`
(`crates/core/src/optimiser.rs::check_constraints`): under `own_label`, `exact` and `ask` it applies
no policy-specific filter, so a product that merely passes the field filters (allowed/excluded
brands, size band, form, dietary, pack) is accepted. The `ask`/`exact` behaviours the household
expects are actually driven by the separate `substitution_policy` (`ask` / `strict`) the same screen
sets.

**Why accepted.** `substitution_policy` already implements the intended substitution semantics
(ask-before-substituting, exact-product-only) and is enforced; adding a parallel policy meaning for
`own_label`/`exact`/`ask` would duplicate it. The screen keeps all six values for schema fidelity and
so policy and substitution can be unified later.

**Workaround.** Use the substitution control for ask/exact behaviour; use `brand` or `allowed_set`
for brand rules.

**Follow-up.** Either collapse the policy vocabulary to the enforced set, or implement `own_label`
and `exact` and derive substitution from the policy.

### 6. The screen exposes preferred size only, not min/max size, pack count, form or dietary flags

**What happens.** `item_constraints` carries `min_size_value`/`max_size_value`, `pack_count`,
`form_factor` and `dietary_flags`, and the optimiser enforces them when set — but the v1 preferences
screen edits only `preferred_size_value` (+ basis), `brand`, `allowed_brands`, `excluded_brands` and
`substitution_policy`. The other fields keep whatever a seed or direct DB write put there; the screen
neither shows nor clears them.

**Why accepted.** The first-class controls requested for v1 were the policy selector, brand rules,
preferred size, rejected products and substitution behaviour; the remaining columns are rarely used
by the household and add UI surface without a driving need.

**Workaround.** None from the app; the values are inherited from the item's existing constraint row
and are not disturbed by a save (the `PUT` upsert leaves those columns untouched).

**Follow-up.** Add controls for min/max size, pack count, form factor and dietary flags, or remove
the unenforced columns.

### 7. Rejecting a product requires an existing match row; there is no product search here

**What happens.** The screen's "Known products" list is exactly the item's `product_matches` rows
(joined to `retailer_products`). Rejecting or restoring a product writes through
`POST /api/match-feedback`. Products the matching helper proposes now appear in the "Suggested
products" section (issue #108) and can be matched or rejected there, but there is no way to search
the catalogue and add an arbitrary retailer product that no label similarity surfaced.

**Why accepted.** Candidate generation (issue #108) covers the household-staples flow; a free-text
catalogue search is a larger surface and was out of scope for both #102 and the version-one matching
helper.

**Workaround.** Match or reject a proposed candidate. If the exact product is not among the
proposals, there is no in-app way to add it yet.

**Follow-up.** Add product search (by name or barcode) so the screen can add any retailer product
directly.

## Product candidate matching

### 1. Barcode assistance is out of scope for version one

Proposes retailer products for an item with no match so a household can accept or reject them in the
app (issue #108). Candidates are ranked by deterministic name/size/brand similarity over the
existing `retailer_products` rows; the LLM fallback (off by default) may only reorder them.

**What happens.** Matching is textual. A household cannot scan a product's barcode to bind it
directly, so a product whose name does not look like the item's name/aliases will not be proposed
even when its barcode would identify it exactly.

**Why accepted.** The captain's version-one scope excludes a mandatory barcode scanner; text, size
and brand similarity cover the staples flow, and `retailer_products.external_id` is not universally
populated across sources.

**Workaround.** None in-app for a product the similarity pass misses; match it only if it appears
among the proposals.

**Follow-up.** Add optional barcode lookup (`retailer_products.external_id`) as a fast path that
surfaces an exact product alongside the ranked candidates. Tracked as a version-one follow-up for
issue #108.

### 2. Matching does not bump the list revision

**What happens.** `needs_match` (issue #108) is computed for list snapshots and current mutation
entities. The preferences flow explicitly reconciles its linked list after a successful match
decision. However, `POST /api/match-feedback` writes `product_matches`, not `list_entries`, so a
match decision emits no SSE `mutation` event; other connected clients can keep a stale prompt until
they fetch and apply an authoritative snapshot.

**Why accepted.** Matching is a catalogue-level, online action independent of the offline list
mutation stream; folding it into the list revision would couple two separate pipelines for a
cosmetic refresh.

**Workaround.** Make the decision from the item's preferences page to refresh the linked list on
that client. Ordinary navigation does not guarantee a refresh on other clients.

**Follow-up.** Bump the list revision or otherwise refresh match coverage when a match decision
changes, so the prompt updates live.

### 3. Candidates are drawn from every store, including retailers the household switched off

**What happens.** Candidate generation ranks every priced `retailer_products` row; it does not filter
by the household's `household_store_settings`. A store the household marked "off" (or "occasional"
with no trip today) can still have a product proposed. Accepting it does not add a plan line while
that store is excluded by the optimiser.

**Why accepted.** Version-one candidate generation runs over the shared catalogue; scoping proposals
to store settings was out of scope.

**Workaround.** Reject the unwanted store's candidate (it does not recur) or leave it unmatched.

**Follow-up.** Filter candidates by the household's active/nearby stores before ranking.

### 4. Candidate ranking repeats on every preferences read and decision

**What happens.** Each preferences read and each accept, reject, or undo decision regenerates
candidates by scanning priced retailer products, so scan and scoring cost grows with the catalogue.
When the LLM fallback is enabled, each such call also makes a paid `rank_candidates` request for the
item.

**Why accepted.** Candidate generation has no cache or explicit rank action in version one.

**Workaround.** Disable the LLM fallback to avoid paid ranking requests; deterministic catalogue
scoring still runs on each preferences read and decision.

**Follow-up.** Reduce repeated catalogue scans and LLM ranking calls without hiding newly available
candidates.

## Purchase suggestions

Estimates, per canonical item, the typical interval between check-offs (median days, minimum three
distinct purchase days) and surfaces items near due in a separate Suggested section on the list
page; confirming adds the item to the list and dismissing pushes the next suggestion one typical
interval later (issue #106, part of roadmap #114). Statistics only — no AI.

### 1. History is check-off driven, household-scoped, and ignores unresolved check-offs

**What happens.** A purchase is recorded only when a RESOLVED list entry is checked off
(`purchase_events`, source `check_off`, written by `apply_mark_bought` in
`crates/api/src/mutations.rs`). Checking off an unresolved or ambiguous entry records nothing,
because there is no canonical item to attribute it to. Evidence is keyed by household, not list, so
an item still active on one list is not excluded from the Suggested section of a *different* list
of the same household. The `purchase_events.source` column exists so a later receipt/import source
can be added, but no such importer is built (a non-goal of this issue).

**Why accepted.** The captain's version-one scope treats check-offs as a proxy for purchases,
distinct from receipt-confirmed purchases, and explicitly excludes importing purchases. Resolved
check-off covers the household-staples flow; resolution normally happens moments after an item is
added.

**Workaround.** Check off items only after they resolve (the list flags unresolved/ambiguous
entries), or add an item manually if a suggestion is missing.

**Follow-up.** Attribute a check-off to its canonical item once resolution completes, add a receipt
or import source, and decide whether suggestions should be scoped per list rather than per
household.

### 2. A dismissal is time-limited, cannot be undone, and does not mean "never suggest this again"

**What happens.** Dismissing a suggestion appends a `purchase_dismissals` row and suppresses the
item until one typical interval has elapsed since the dismissal, after which it is suggested again.
The table is append-only and there is no UI to un-dismiss or to permanently opt an item out.

**Why accepted.** The captain's scope is "dismissals push the next suggestion later"; a permanent
opt-out and an undo are larger product surfaces left to a follow-up. Resurfacing next cycle matches
the "suggest what the household is likely to need" intent.

**Workaround.** Dismiss again if the item is still not wanted, or simply ignore the suggestion —
nothing is added to the list without confirmation.

**Follow-up.** Add a "don't suggest this again" (permanent opt-out) option and a way to review or
undo dismissals.
