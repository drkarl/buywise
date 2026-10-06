# Known Limitations

Shipped features record their known limitations here, in the repository, instead of only in the pull
request that introduced them — merged PR descriptions are lost and are not a place operators search.
Add one section per feature when a ship carries a known limitation. Each entry gives the limitation,
what happens, why it was accepted, a workaround or none, and the follow-up that removes it (a tracked
issue, a backlog id, or an explicit open question). A limitation that must hold a release is marked
**RELEASE BLOCKER** at the top of its section.

_Last reviewed: 2026-10-06._

## Per-store trip view

Splits the chosen optimiser plan into one shopping list per store — the trip view, the store-scoped
shopping mode, and the allocation backing them (issue #104, merged in
[PR #125](https://github.com/drkarl/buywise/pull/125)). Design: `docs/design.md` §10.

### 1. The chosen trip is a per-device cache, and an entry added after choosing has no per-item view

**What happens.** The chosen allocation is a client-side cache in `localStorage` under
`buywise:trip:<listId>` (`frontend/src/lib/shopping/chosenTrip.ts:40`, `:62`), not server state. It is
therefore per device: another household device does not see the choice until it picks a plan itself
— its trip page shows "No plan selected yet" (`trip/+page.svelte:149-152`) and its shopping mode
falls back to all five stores. The cache is never invalidated by app code (`clearChosenTrip`,
`chosenTrip.ts:71`, is exported but only tests call it); it only changes by re-selecting a plan on
the optimise page, which overwrites it (`optimise/+page.svelte:223-249`), and there is no per-line
manual reassignment.

Both the trip page (`trip/+page.svelte:155-157`) and store-scoped shopping mode
(`shopping/+page.svelte:152-154`) now show the shared stale-plan warning
(`frontend/src/lib/components/StalePlanWarning.svelte`) with the review path back to the optimiser.
It is driven by `isTripStale` (`chosenTrip.ts:93-106`), which flags an `active` entry absent from the
plan and a planned entry whose quantity differs from the snapshot taken when the plan was chosen
(the `quantities` map on the cached trip, populated by `choosePlan` in `optimise/+page.svelte`).
It also flags a planned entry whose canonical item changes after the plan was priced
(`canonicalItemIds` on the cached trip); older caches check only fields they contain.
The store-scoped view still renders only the plan's allocated lines (`storeEntryIds`,
`shopping/+page.svelte:80`), so an entry added after choosing is counted by the "N item(s) not in a
store list — View whole trip" link (`shopping/+page.svelte:85-93`, `:156-166`) but has no per-item
view in either the trip or store-scoped shopping view — the trip's "Not allocated to a store"
section (`trip/+page.svelte:182-206`) carries only the items the plan itself left unallocated. The
entry remains available on the plain list page (`frontend/src/routes/(app)/list/[id]/+page.svelte:223-249`),
where it can be checked off. The per-store `Estimated` totals (`trip/+page.svelte:163`) remain the
plan-time prices, not a live re-price.

**Why accepted.** The per-store lists are views over the shared list entries and every check-off
still flows through the normal offline queue, so devices converge on the entries even though each
holds its own plan snapshot. `localStorage` avoids a Dexie schema migration for a small, per-device
snapshot, and design §10 records the per-device cache, the replace-by-reselecting workflow, and the
frozen-price snapshot as deliberate volume-one scope. The stale warning now covers added entries,
quantity edits, and canonical-item changes without moving the plan to server state; giving a
later-added entry its own per-item view inside the trip views, and sharing the chosen plan between
devices, are larger changes left to follow-ups.

**Workaround.** Open `/list/<id>` to view and check off an entry added after choosing a plan. Open
`/list/<id>/trip` for the whole trip — its stale banner links back to the optimiser. Re-optimise and
re-choose on each device; the shared entries and their statuses stay in sync.

**Follow-up.** Decide whether the chosen plan becomes shared server state so every device sees the
household's choice. Add a per-item view for entries added after choosing inside the trip views. Add
per-line reassignment if the workflow needs it.

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

## Per-store category rules

Rules that keep a store out of plan generation for chosen categories (issue #99, the Aldi
packaged-food case). Storage: `household_store_category_rules` (migration 014); enforcement in
`crates/core/src/optimiser.rs`; settings surface at `frontend/src/routes/(app)/settings/+page.svelte`.

### 1. Category rules apply only to plan generation

**What happens.** The optimiser will not assign an item to a retailer whose rules exclude its
category, but nothing else in the product consults the rules. Product-candidate matching
(`crates/core/src/matching/`) still proposes candidates from an excluded category at that store, and
shopping mode still groups an item under its normal aisle/category section regardless of the rule.
So a household can accept a match for a fresh item at Aldi and still never see it priced there.

**Why accepted.** Issue #99 scopes the rule to the optimiser ("the optimiser never assigns an item
to a store whose rules exclude its category"); candidate generation and shopping mode were not in
scope. Every rule the household sets is still enforced where it matters — the plan.

**Workaround.** Read the plan's reasons: a store skipped by a rule reports "skipped for N items by
your store category rules" on the recommended card. Reject an excluded store's candidate on the item
preferences screen if its proposal is unwanted (it does not recur).

**Follow-up.** Filter product-candidate proposals by the same per-store category rules, and (if the
workflow needs it) reflect the rules in shopping-mode grouping.

### 2. An item the optimiser cannot classify is never excluded

**What happens.** An item whose category row has gone missing is allowed at every store, so a
category rule cannot exclude it. The optimiser only receives an item's category when the API lookup
finds its canonical item; an unresolved entry is already excluded from pricing for other reasons.

**Why accepted.** Dropping an unclassifiable item would silently remove it from every plan, which is
worse than pricing it where we can. The catalogue maps every seeded item (migration 010 backfills
sub-categories; 002 gives every item a category), so this only affects hand-inserted or future rows.

**Workaround.** None needed for the seeded catalogue; check `canonical_items.category_id` if a
custom item is not being excluded as expected.

**Follow-up.** Surface an unclassified resolved item as a data warning rather than only defaulting it
to allowed.

## Aldi comparison

See [the design document](design.md#aldi-comparison-issue-100) for comparison behavior and data
semantics. The remaining limitations are:

### 1. The savings page does not aggregate the Aldi figure into a running total yet

**What happens.** Each optimisation records the comparison inside `recommendation_runs.plans_json`,
so the per-run figure survives, but `GET /api/savings/summary` only aggregates `savings_events` and
does not read `plans_json`. The savings page therefore shows no running Aldi total over weeks.

**Why accepted.** Folding the figure into the savings dashboard would mean either adding Aldi
columns to `savings_events` and populating them at archive time, or parsing every
`recommendation_runs.plans_json` for the household's lists — a broader change than the issue's
optional recording, which the intent scoped to "a small migration or leave it out". The figure is
still persisted per optimisation for a later pass.

**Workaround.** Read the comparison on each optimisation run; the per-run figure is in the
optimise response and in the stored `plans_json`.

**Follow-up.** Issue #100 follow-up: add `aldi_saving_packaged_cents` / `aldi_saving_fresh_cents`
to `savings_events` (populated when a plan is chosen or the list is archived) and show the running
total on `/savings`.

### 2. An item with no category, or a category not marked fresh, counts as packaged

**What happens.** The fresh/packaged split reads `categories.is_fresh` (Produce, Meat & Fish, Dairy
and Bakery are fresh). A canonical item whose category row is missing, a category a household adds
later, and every item the CLI costs in fixture mode (no taxonomy) default to packaged. A genuinely
fresh item in such a category is reported under "packaged food" instead of "fresh food".

**Why accepted.** Defaulting an unclassifiable item to fresh would overstate the figure the quality
concern is about; treating it as packaged keeps the fresh number a lower bound. The seeded catalogue
maps every item to a category (002), so only hand-inserted, future or CLI items are affected.

**Workaround.** Mark the category fresh (`UPDATE categories SET is_fresh = 1 WHERE id = ...`) if a
new category genuinely holds fresh food.

**Follow-up.** Expose `is_fresh` on the settings catalogue so a household can set it without SQL,
and make the CLI's fixture mode load the category taxonomy.

## Savings ledger

Issue #117, roadmap #114. The status model, accepted rules and the three decisions are in
[`docs/design.md`](design.md) §10 ("Savings ledger and honest labels"). Migration 016 adds the
`savings_events.status` column. The remaining limitations are:

### 1. Nothing is verified yet — there is no line-level reconciliation

**What happens.** `GET /api/savings/summary` counts only `verified` events as saved, and no
production path writes `verified` yet (it needs the purchase ledger of #116). The dashboard's
verified headline therefore reads €0.00 with an empty state. See [`docs/design.md`](design.md) §10
for the authoritative status meanings and display rules.

**Coverage history.** Posting an actual spend replaces the row's earlier status, so an
`unreconciled` or `unchecked` row becomes `legacy_total_only`. Migration 016 also labels every
pre-existing row with an actual-spend total `legacy_total_only`, regardless of its earlier
coverage. Retaining revisions of the earlier status belongs to the later event-schema slice.

**Why accepted.** Issue #117's slice is honest labels and a verified-only aggregation guard;
automatic line reconciliation is explicitly deferred until the purchase-ledger experiment
supports it.

**Workaround.** Use each event's status and coverage label as described in [`docs/design.md`](design.md)
§10; only `verified` events represent saved money.

**Follow-up.** #116/#117: reconcile planned versus purchased quantities/products (extra, skipped,
substituted, unmatched lines) to produce `verified` events.

### 2. The snapshot is frozen for new runs only, and it freezes prices, not purchases

**What happens.** Each run created since migration 018 freezes its input basket, the offers it used
and the coverage result, so a later list edit or offer refresh cannot change it (see
[`docs/design.md`](design.md) §10, "Frozen run evidence and the covered subtotal"). Two gaps
remain: runs created **before** migration 018 keep NULL snapshot columns (they stay readable
everywhere, but carry no frozen evidence), and the snapshot records the planned line/pack inputs and
prices only — it does not reconcile what was actually bought.

**Why accepted.** The slice's scope is freezing the evidence a run was computed from; purchase
reconciliation is #116. Backfilling old runs is impossible — the input they used no longer exists —
so they are deliberately left NULL rather than reconstructed from mutable data.

**Workaround.** Use a new run's snapshot for price and input evidence; for a pre-018 run the stored
totals remain the only record.

**Follow-up.** #116/#117: reconcile planned versus purchased lines and quantities on top of the
frozen snapshot. #109: populate `usual_profile_json` with the household's written usual-shop quotes.

The retailer-product ingest contract does not define whether `size_value` is per unit or per pack.
Offer evidence therefore freezes `size_value`, `size_unit` and `pack_count` as stored, while derived
usable base units remain NULL. The later reconciliation slice needs this contract defined before it
can derive total usable units.

### 3. No usual-shop baseline

**What happens.** The best single store estimate remains the comparison anchor for stored events.
The household's written "usual shop" profile — recorded as the primary baseline for the live
evaluation in `docs/design.md` §10 — is not implemented, so no saving is measured against what the
household would normally have spent. `recommendation_runs.usual_profile_json` (migration 018) is
reserved for these quotes and stays NULL.

**Why accepted.** This slice adds no new baselines.

**Workaround.** None; the dashboard always names the baseline it does use ("Baseline: best single
store estimate").

**Follow-up.** #109: implement the usual-shop written profile as the primary live-evaluation
baseline and populate `usual_profile_json`.

### 4. No receipt links and no automatic purchase matching

**What happens.** Purchase records and receipts are not linked to recommendations or lists; the
only realised input remains a manual whole-basket total, which is why every reconciled-by-hand
event is `legacy_total_only`.

**Why accepted.** Receipt capture is a bounded validation experiment (#116), not a prerequisite
for the manual evaluation.

**Workaround.** Enter actual spend manually to get a legacy basket variance; it is labelled as
unverified.

**Follow-up.** #116 purchase ledger: link receipts/purchases to lists and recommendations, and
support multiple receipts per shop without double-counting.

### 5. The coverage guard compares priced lines, not quantities or pack sizes

**What happens.** The archive-time guard (`optimiser::prices_same_lines`) compares the set of
basket item indices both plans price — the same rule the optimise explanation and the covered
subtotal use to refuse a whole-basket cost comparison. Plan lines carry no quantity, so buying a
larger pack or extra units is not detected as a coverage difference; the strict pack-size decision
recorded in `docs/design.md` §10 is not enforced in this slice.

**Why accepted.** Quantities are not part of a stored plan line; enforcing the pack-size rule needs
the line-level reconciliation of the follow-up work.

**Workaround.** Treat an `estimated` row as a line-coverage-only opportunity estimate; an
`unreconciled` row shows only the bounded `Covered subtotal: N of M lines` figure, which is likewise
line-scoped.

**Follow-up.** Include quantities and equivalent pack sizes in the comparison when reconciliation
lands (issue #117); the frozen basket and offer snapshots (migration 018) already carry the pack
counts and sizes this will need.

## Household data controls

Issue #119, roadmap #114. The authoritative policy, behaviour, procedures and export layout are in
[`docs/data-flows.md`](data-flows.md) and [`docs/household-export.md`](household-export.md).

Open follow-ups:

- When receipt ingestion is implemented, enforce a per-upload processing choice, provider
  disclosure, no-publication/no-training terms, image expiry and deletion.
- Decide backup encryption and key custody; verify the deployed R2 token scope and record a restore
  rehearsal (retention is now explicit at 7 days and the rehearsal is a required runbook step —
  issue #142).
- Consider recording deletion tombstones for post-restore reapplication, and a local vacuum option
  for deleted SQLite pages.
- Revisit deletion of aggregate usage counters if they become household-attributable.
- Consider an encrypted export option for the point-in-time JSON snapshot.

## HTTPS reverse proxy (deploy runbook)

Issue #140, roadmap #114 (deploy issue #96). Caddy terminates TLS in front of the
BuyWise API/SPA; see `deploy/README.md` section 7 and `deploy/Caddyfile`.

### 1. The API hardcodes its bind address, so port 3000 is kept private by the host firewall rather than by the app

**What happens.** `buywise-api` binds `0.0.0.0:PORT` unconditionally
(`crates/api/src/main.rs`: `SocketAddr::from(([0, 0, 0, 0], port))`); only the
port is configurable (`PORT`). The process therefore listens on every interface,
including the public one, and the deploy runbook relies on the host firewall
(ufw: SSH/80/443 only) to keep port 3000 unreachable from the internet. The
inside-the-host port stays reachable to any local process, and the listener is
not "closed" at the socket level — it is only blocked at the packet filter.

**Why accepted.** Binding the API to loopback is the clean fix, but it needs a
code change to read a bind address from the environment, and this issue's scope
is documentation and deploy configuration only. The firewall is the captain's
chosen control ("a firewall allowing SSH and HTTPS only with the API port closed
to the public"), and it is sufficient on this single-host deploy where Caddy is
the only public listener.

**Workaround.** Keep the firewall rules as documented in `deploy/README.md`
section 7.2 and confirm `sudo ufw status verbose` lists only SSH, 80 and 443.

**Follow-up.** Add a bind-address environment variable (e.g. `BIND_ADDRESS`,
defaulting to `0.0.0.0`) to `crates/api/src/main.rs` and set it to `127.0.0.1`
in `deploy/systemd/buywise.service`, so the port is loopback-only without
depending on the firewall.

## Household member management

Issue #139, roadmap #114. An owner adds and removes members from **Settings → Household members**
(or the API), and `bootstrap-household` creates the very first household and owner on a fresh
database. The roles (`owner` / `member`) are enforced: export stays available to every member, while
member administration and the household delete require the owner role. The endpoints and rules are
in [`docs/data-flows.md`](data-flows.md) (§5.1a).

Open follow-ups:

- **Roles cannot be changed.** There is no promote/demote action, so an owner cannot hand ownership
  to another member or demote a co-owner; the last-owner guard only prevents *removing* the final
  owner. A household that needs to move ownership must edit `household_members.role` in the
  database. Add promote/demote when ownership hand-over is required.
- **Bootstrap is one-time and single-household.** `bootstrap-household` refuses once any account
  exists, so creating a second household — and re-running it after a partial setup — is out of
  scope. There is still no self-registration or email invite flow (both deliberate non-goals).
- **Removing a member notifies nobody.** Their sessions end immediately and their account is
  deleted only if they belong to no other household, but no email is sent and list data already
  cached in their browser (IndexedDB/localStorage) is not reachable from the server — the same
  limitation that applies to a deleted household.

## Deploy health check and smoke test

Issue #141, roadmap #114 (deploy issue #96). The API exposes an unauthenticated `GET /api/health`
(database reachable, migrations applied) and `deploy/smoke.sh` checks health, the PWA
manifest/service worker and — with credentials — auth and a short SSE connection.

### 1. The health endpoint only proves the database and schema are ready

**What happens.** `crates/api/src/health.rs` answers `200 {"status":"ok"}` after one trivial query
and a check that every embedded migration is recorded as applied; otherwise
`503 {"status":"unavailable"}`. It does not check the frontend build, the R2 replica, JSON1
(asserted once at startup), disk space, or that the database file is writable. A deploy that serves
a stale or missing `frontend/build` still reports healthy.

**Why accepted.** The probe answers one question — may this process serve requests against a merged
database — and must stay cheap and safe to expose without a session. Deeper checks belong in the
human-run smoke test; growing the endpoint into a monitoring surface is a non-goal of issue #141.

**Workaround.** Run `deploy/smoke.sh <base-url>`, which additionally proves the frontend assets are
served and (with credentials) that login, an authenticated list read and the SSE stream work.

**Follow-up.** If monitoring is ever wanted, add the extra checks to the smoke script or a separate
probe rather than widening `/api/health`.

### 2. The smoke script's authenticated checks are skipped when credentials or a list are absent

**What happens.** `deploy/smoke.sh` always performs the health and PWA-asset checks, but the
authenticated list read and the SSE connection need `BUYWISE_SMOKE_EMAIL`/`BUYWISE_SMOKE_PASSWORD`;
when the account can see no list, the SSE check is skipped with a `SKIP:` line
(`BUYWISE_SMOKE_LIST_ID` forces a target). A credential-less run therefore passes without exercising
auth or live sync. The SSE wait is bounded (35 s default) and a bare frame is the only assertion —
the stream's contents are not validated.

**Why accepted.** The script must be runnable on a fresh host before any account exists, and a smoke
test is a readiness gate rather than an end-to-end suite.

**Workaround.** Run it with a real account after `bootstrap-household` (runbook section 6.1) to
cover the authenticated paths.

**Follow-up.** None planned; extend only if a specific failure mode needs it.

## Backup retention and R2 token scope

Issue #142, roadmap #114 (deploy issue #96). `deploy/litestream.yml` sets `retention: 168h`
(7 days), and the runbook documents a bucket-scoped R2 token and a required restore rehearsal.

### 1. The R2 token reaches every object in the bucket, not just Litestream's prefix

**What happens.** Litestream writes under the `db` prefix, but Cloudflare R2 scopes an API token to
whole buckets, not object prefixes (`Object Read & Write` applied to `buywise-backup`). The token
can therefore read, write and delete any object in `buywise-backup`, including anything else the
bucket later holds.

**Why accepted.** Bucket scope is the narrowest R2 offers; the owner chose it over an account-wide
token (issue #136), and the bucket holds only the replica. Client-side encryption remains a
non-goal for now (provider-managed keys).

**Workaround.** Keep `buywise-backup` dedicated to the Litestream replica.

**Follow-up.** Revisit if the bucket must hold anything else, or when encryption with a
household-held key is reconsidered (issue #136).

### 2. A restore point can resurrect rows deleted up to 7 days ago

**What happens.** With `retention: 168h` the replica keeps seven days of history, so a row — or a
whole household — deleted on the live server can be resurrected by a disaster restore until those
frames age out. Deletion is not immediate in the backup. Shortening retention shrinks that window
but also shortens the recovery window for accidental deletions.

**Why accepted.** Seven days is the owner's explicit choice (issue #136): long enough to recover a
late-noticed deletion, short enough to bound how long deleted data survives in R2.
`docs/data-flows.md` §5.3 documents the reapply-after-restore sequence.

**Workaround.** After a restore that predates a deletion, re-run
`buywise-cli delete-household --confirm`; to erase the frames sooner, purge the `db` prefix and let
Litestream re-seed (this drops all older recovery points).

**Follow-up.** Deletion tombstones, so a delete can be reapplied automatically after restoring an
older backup (issue #136).

### 3. The restore rehearsal is manual, and its record lives outside the repository

**What happens.** The runbook requires a non-destructive restore rehearsal (Disaster recovery →
Restore rehearsal) with a result table, but nothing runs or checks it automatically:
`validation-interval` is unset, so Litestream never auto-verifies a restore, and the recorded result
is a table the operator fills in outside this repository (the issue or deploy log). A deploy that
skipped the rehearsal looks the same in the repository as one that passed.

**Why accepted.** Automatic validation costs egress on every run, and the rehearsal is a human
procedure; the runbook now makes it a required step with a place to record evidence.

**Workaround.** None — run the rehearsal and record the result.

**Follow-up.** Consider `validation-interval` once egress cost is acceptable, or persist rehearsal
evidence somewhere durable (issue #136).

## First deploy without the price ingest

Issue #143, roadmap #114 (deploy issue #96). `setup.sh` installs the ingest units but does not
enable `buywise-ingest.timer`, and the runbook says when and how to enable it.

### 1. Price comparison stays empty until the ingest is enabled and matches are seeded

**What happens.** A fresh deploy has no retailer offers and no `product_matches` rows, so the
optimiser has nothing to price and the app shows no prices. The weekly ingest is installed but
disabled, and once enabled it needs `BASKETWATCH_API_KEY`; `LIDL_FEED_URL` has no default and there
is no verified Lidl feed. Enabling the timer is a deliberate later operator step, after the
live-savings validation (issue #109) confirms supplier access, terms and cost.

**Why accepted.** The owner decided to deploy without the price ingest (issue #143): the supplier
terms and cost are unvalidated, and a timer enabled without the key reds the unit. The API and the
shared lists work fully without prices.

**Workaround.** Seed `product_matches` with `buywise-cli seed-matches` or accept in-app proposals,
and enable the ingest per "Weekly price ingest" once issue #109 is resolved. Until offers exist,
expect an empty price comparison.

**Follow-up.** Enable the ingest after issue #109, add a Lidl feed only once one is verified; #109
also decides whether the supplier is retained at all.
