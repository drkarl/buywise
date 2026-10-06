# Household Grocery Optimiser — Design Document

**Status:** Draft v13 — eleventh Codex review applied  
**Household:** 2 adults, 2 children, Ireland  
**Purpose:** Private household utility; not a public product

---

## 1. Purpose

A private grocery optimisation app for one household. Not a price-comparison site. The job is to answer:

> Given what the household needs to buy, its brand/size/form-factor constraints, acceptable substitutions, and current Irish supermarket prices — where should we shop, what should we buy, and is it worth splitting the basket across more than one store?

Target supermarkets: Tesco, Dunnes Stores, SuperValu, Aldi, Lidl.

---

## 2. Success Criteria

The app is worth building only if it saves money beyond its running cost:

| Outcome | Monthly net saving |
|---|---|
| Minimum acceptable | €100 |
| Good | €150–€250 |
| Excellent | €300+ |

The app must track actual savings (gross theoretical, accepted practical, net of API and travel cost) so the household can judge whether the effort and subscription are justified.

**First milestone before writing application code:** build a CLI validation tool that estimates savings using real BasketWatch data against a reconstructed typical weekly shop. If savings are below €50/month realistically, stop.

---

## 3. Users

Two users: a couple. Both have equal rights to view and modify the shared list. Household includes two children (relevant for constraints: nappy sizes, child-specific items).

The data model should use `households / users / household_members` — not hardcoded two-user assumptions — but multi-household SaaS is not a goal.

Auth: two seeded user records (no public registration). **Password login for MVP.** Magic-link is a later option but requires SMTP configuration, email deliverability, and an additional failure mode — not appropriate for a Phase 1 private appliance. No OAuth. Per-user attribution matters for sync, matching feedback, and household learning ("who rejected this suggestion?").

**Session and CSRF design:** login issues two cookies and stores `csrf_token` in the `sessions` table:
- `session` — `HttpOnly; Secure; SameSite=Strict`; contains the opaque session token (JS cannot read it)
- `csrf` — readable (no `HttpOnly`), but still `Secure; SameSite=Strict`; contains the CSRF token bound to that session

For every state-changing request (POST/PUT/DELETE), the client reads `document.cookie` for `csrf`, places it in an `X-CSRF-Token` header, and the server validates the header value against the `csrf_token` stored in the session record. This is a **session-bound CSRF token** (synchronizer token via readable cookie + header) — not the classic stateless double-submit pattern, which does not validate against a server-side record. `SameSite=Strict` on the session cookie already blocks most cross-site request forgery; the session-bound token adds explicit per-session binding as defence-in-depth. The `sessions` table gains a `csrf_token` column.

---

## 4. Guiding Principle

**The household catalogue is the source of truth.**

External supermarket feeds are replaceable price sources. The system is designed around household decision-making, not around the structure of any one API. The conceptual pipeline:

```
raw user input
  → canonical household item
  → household constraints & preferences
  → acceptable retailer product candidates
  → current/cached offers
  → basket optimisation
  → practical recommendation
  → user feedback
  → improved future matching
```

---

## 5. Tech Stack

All decisions are settled unless noted.

| Layer | Choice | Rationale |
|---|---|---|
| Frontend | SvelteKit PWA | Good offline story, small bundle, TypeScript, installable on mobile |
| Offline storage | Dexie (IndexedDB) | Mutation log and local list state |
| Realtime push | SSE (server-sent events) | Simple, Axum-native, sufficient for 2 users |
| Backend | Rust + Axum | Low memory footprint for cheap hosting; user has Rust experience; good for ingestion/optimisation |
| ORM | sqlx | Compile-time checked queries, async, SQLite support |
| Database | SQLite (WAL mode) | Zero ops, single file, handles 2-user concurrency comfortably |
| Backup | Litestream → Cloudflare R2 | Continuous WAL replication; R2 free tier (10 GB) is sufficient |
| Hosting | Hetzner CAX11 ARM (~€5.34/month) | Predictable, cheap, single binary + SQLite volume; always-on |
| Static serving | tower-http ServeDir | Serve SvelteKit static build from same Axum binary; SvelteKit uses `adapter-static` in SPA mode — no SvelteKit server routes in production, all data via Axum API |

**Not used:** Supabase (free tier pauses after inactivity — fatal for in-store use; Pro is €25/month), Neon (unnecessary complexity at this scale), Render free tier (ephemeral storage, 30-day Postgres expiry), Vercel (not needed without Next.js), Fly.io (billing changed; no clean free tier).

**Cloudflare R2 free tier:** 10 GB-month storage, sufficient Class A/B operations for Litestream. Zero egress cost.

---

## 6. Architecture

```
┌─────────────────────────────────────────────┐
│             SvelteKit PWA                   │
│  ┌────────────────────────────────────┐     │
│  │  Service Worker (app shell cache)  │     │
│  │  Dexie / IndexedDB                 │     │
│  │    - list_items (local mirror)     │     │
│  │    - mutation_queue (pending ops)  │     │
│  └────────────────────────────────────┘     │
│  Online: SSE stream ← server push           │
│  Offline: queue mutations, sync on reconnect│
└───────────────────┬─────────────────────────┘
                    │ HTTPS + SSE
┌───────────────────▼─────────────────────────┐
│            Rust / Axum API                  │
│                                             │
│  POST /api/mutations    (apply + broadcast) │
│  GET  /api/list/:id     (full state)        │
│  GET  /api/sse/:list_id (event stream)      │
│  POST /api/optimise     (basket plans)      │
│  GET  /api/items        (canonical catalogue)│
│  POST /api/auth/*       (login / session)   │
│                                             │
│  Background jobs (tokio-cron-scheduler):    │
│    - Weekly ingestion cron                  │
│      └─ BasketWatch adapter                 │
│      └─ Lidl adapter                        │
│      └─ Normalise → product_offers table    │
│    - API usage guardrail checks             │
│                                             │
│  SQLite WAL (app.db)                        │
└───────────────────┬─────────────────────────┘
                    │ Litestream WAL tail
          ┌─────────▼──────────┐
          │  Cloudflare R2     │
          │  (backup storage)  │
          └────────────────────┘

Ingestion sources (weekly, background):
  BasketWatch via RapidAPI  →  retailer_products + product_offers
  Lidl.ie direct parser     →  retailer_products + product_offers
```

### Deployment

Single binary: `grocery-optimizer`. Serves the SvelteKit PWA static files and the Axum API on the same port. SQLite file on a persistent volume. Two systemd services run side by side: the app binary and a separate `litestream` process. Do not embed Litestream in the app — running it as a supervised sibling process keeps recovery straightforward and matches standard Litestream deployment.

---

## 7. Data Model

### Core entities

```sql
-- Household and users
households          (id, name, created_at)
users               (id, email, password_hash, display_name, created_at)
household_members   (household_id, user_id, joined_at)
sessions            (id, user_id, token_hash, csrf_token, created_at, expires_at)

-- Shopping list
shopping_lists      (id, household_id, name, status,
                     current_revision,  -- list-scoped monotonic counter; incremented on every accepted mutation
                     created_at, archived_at)
list_entries        (id,                         -- client-generated UUID v4; assigned by the client before the add_item
                                                 -- mutation is sent, so it is stable and known offline; the server
                                                 -- uses it as-is (no server-assigned surrogate) — this is also what
                                                 -- allows 'unresolved:<entry_id>' to be computable without a round-trip
                     list_id,
                     canonical_item_id,          -- NULLABLE: NULL until the entry is resolved
                     resolution_status,          -- unresolved | resolved | ambiguous  (default: unresolved)
                                                 -- invariants (enforce with SQLite CHECK constraints where practical;
                                                 --   SQLite cannot add CHECK constraints to existing tables via ALTER TABLE,
                                                 --   so these must be defined in the initial CREATE TABLE migration):
                                                 --   resolved  → canonical_item_id IS NOT NULL, resolution_candidates_json IS NULL
                                                 --   unresolved→ canonical_item_id IS NULL,     resolution_candidates_json IS NULL
                                                 --   ambiguous → canonical_item_id IS NULL,     resolution_candidates_json IS NOT NULL (non-empty array)
                                                 -- CHECK(resolution_status IN ('unresolved','resolved','ambiguous'))
                                                 -- resolved ↔ canonical_item_id IS NOT NULL (both directions):
                                                 -- CHECK(resolution_status != 'resolved' OR canonical_item_id IS NOT NULL)
                                                 -- CHECK(canonical_item_id IS NULL OR resolution_status = 'resolved')
                                                 -- unresolved → candidates must be NULL:
                                                 -- CHECK(resolution_status != 'unresolved' OR resolution_candidates_json IS NULL)
                                                 -- ambiguous → candidates must be present:
                                                 -- CHECK(resolution_status != 'ambiguous' OR resolution_candidates_json IS NOT NULL)
                                                 -- ambiguous → non-empty array (requires JSON1; omit if not available):
                                                 -- CHECK(resolution_status != 'ambiguous' OR json_array_length(resolution_candidates_json) > 0)
                                                 -- resolved → no candidates:
                                                 -- CHECK(resolution_status != 'resolved' OR resolution_candidates_json IS NULL)
                     resolution_candidates_json, -- NULLABLE JSON array of candidate canonical_item_ids (ambiguous only)
                     raw_text,
                     display_name,               -- normalised display form (e.g. "leche semidesnatada 2L" → "Milk semi-skimmed 2L")
                     added_by, quantity,         -- quantity default 1; incremented on cross-device duplicate add
                     status,                     -- active | bought | skipped | deleted
                     bought_by,                  -- user_id who checked it off
                     bought_at,                  -- when it was checked off
                     effective_constraint_hash,  -- NOT NULL; stable hash of the resolved constraint set for this entry
                                                 -- sentinel values:
                                                 --   'unresolved:<entry_id>' → canonical_item_id IS NULL (entry not yet
                                                 --                   resolved); entry_id makes it unique per row, avoiding
                                                 --                   any future footgun if the partial index predicate
                                                 --                   (canonical_item_id IS NOT NULL) is ever relaxed
                                                 --   'default'    → resolved entry with genuinely empty constraint set
                                                 --   SHA-256 hex  → normal resolved entry with real constraints
                                                 -- SQLite UNIQUE allows multiple NULLs — NOT NULL prevents that trap
                                                 -- hash rule: SHA-256 of sorted-key JSON of resolved constraints after
                                                 --   unit normalisation; exclude non-semantic fields (timestamps, user ids)
                                                 --   e.g. {"brand":null,"dietary":[],"min_size_g":null,"pack_count":null,...}
                                                 -- both clients must use the same serialisation to produce the same hash
                     last_updated_revision,
                     updated_at)
                     -- unique index: (list_id, canonical_item_id, effective_constraint_hash)
                     --   WHERE status = 'active' AND canonical_item_id IS NOT NULL
                     -- unresolved entries (canonical_item_id IS NULL) are excluded from dedup and never merged
                     -- "milk" and "milk x2" → same resolved constraints → same hash → merge (increment quantity)
                     -- "milk" and "lactose-free milk" → different constraints → different hash → two rows
list_entry_events   (id, entry_id, user_id, event_type, payload, created_at)

-- Offline sync
mutations           (id,                   -- server-assigned PK (auto-increment integer)
                     client_mutation_id,   -- client UUID; unique index; idempotency key
                     device_id, user_id, list_id,
                     entity_type, entity_id,
                     operation_type,  -- add_item | mark_bought | mark_skipped | delete_item
                                      -- rename_item | resolve_alias | reject_match | set_constraint
                                      -- set_quantity
                     payload,         -- JSON
                     base_revision,   -- revision client was at when mutation was created
                     created_at_client,
                     received_at_server,
                     server_revision, -- assigned on acceptance
                     status)          -- accepted | rejected | merged

-- Household catalogue
canonical_items     (id, household_id, name_en, name_es,
                     category_id,                -- FK → categories.id
                     default_constraint_policy,  -- any | brand | allowed_set | own_label | exact | ask
                     created_at)
item_aliases        (id, household_id, canonical_item_id, alias, language, source,
                                      -- source: manual | accepted_match | llm
                     created_at)
                     -- unique index: (household_id, lower(alias), language)
                     -- household_id enables household-scoped lookup without joining through canonical_items
item_constraints    (id, canonical_item_id,
                     -- optional per-item overrides
                     brand, allowed_brands, excluded_brands,
                     -- size constraints stored in canonical base units to avoid unit-mismatch bugs:
                     --   weight  → grams (g);  volume → millilitres (ml);  discrete → count (each)
                     -- the normaliser converts retailer product sizes to the same base before comparison
                     -- (e.g. retailer "2L" → 2000 ml; "1kg" → 1000 g; "40 sheets" → 40 each)
                     preferred_size_value, preferred_size_basis,  -- e.g. 2000, ml  or  1000, g  or  1, each
                     min_size_value, min_size_basis,
                     max_size_value, max_size_basis,
                     -- covers nappies (each), pods (each), washes (each), sheets (each), rolls (each), cans (each)
                     pack_count, form_factor, dietary_flags,
                     substitution_policy,  -- any | ask | strict
                     updated_at)

-- Categories and store-specific ordering (ported from Shoppy)
categories          (id, name_en, name_es, icon, default_order)
                     -- seeded with 11 categories: PRODUCE, MEAT_FISH, DAIRY, BAKERY, PANTRY,
                     --   FROZEN, SNACKS, DRINKS, BABY, HOUSEHOLD, TOILETRIES
                     -- canonical_items.category_id FK → categories.id
subcategories       (id, category_id, name_en, name_es, default_order)
                     -- issue #101: one optional split level beneath a broad category, so
                     --   shopping mode shows several smaller sections (e.g. Pantry → Pasta &
                     --   Rice, Tins & Jars, Sauces, Breakfast, Baking, Spices). Shared
                     --   reference data like `categories`, bilingual, ordered within its
                     --   parent by default_order. Seeded by migration 010.
                     -- canonical_items.subcategory_id FK → subcategories.id (NULLABLE: NULL
                     --   means shopping mode falls back to the parent category)
store_category_orders (id, household_id, store,    -- tesco | dunnes | supervalu | aldi | lidl
                       category_id, display_order,  -- per-store aisle ordering for shopping mode
                       updated_at)
                     -- unique(household_id, store, category_id)
                     -- default ordering seeded per store
                     -- orders PARENT categories only; per-store sub-category layouts are
                     --   out of scope (issue #101), sub-categories use their default_order

-- Price data
retailer_products   (id, source,          -- basketwatch | lidl_direct | manual
                     retailer,            -- tesco | dunnes | supervalu | aldi | lidl
                     external_id, url,    -- external_id: NULL for manual products (no upstream ID)
                     name, brand, category,
                     pack_size_text,
                     size_value, size_unit,
                     pack_count,
                     first_seen_at, last_seen_at)
                     -- deduplication strategy differs by source:
                     --   API sources (basketwatch, lidl_direct): partial UNIQUE index on (source, retailer, external_id)
                     --     WHERE source != 'manual' AND external_id IS NOT NULL
                     --     SQLite allows multiple NULLs in a UNIQUE index, so the predicate must exclude NULL external_ids;
                     --     ingestion code must reject any API product row with a missing external_id (treat as parse error)
                     --     rather than inserting it and silently bypassing dedup
                     --   manual products: external_id IS NULL; distinct by primary key; UI prevents obvious duplicates

product_offers      (id, retailer_product_id,
                     price_cents, unit_price_eur_micros, unit_price_basis,  -- money as integers: price in euro cents (×100),
                                                                           -- unit_price_eur_micros = €/unit × 1_000_000 for precision
                                                                         -- per_kg | per_l | per_each | per_sheet ...
                     previous_price_cents,
                     loyalty_price_cents, loyalty_scheme,  -- clubcard | real_rewards | null
                     offer_text, offer_valid_until,
                     source_snapshot_date,
                     fetched_at, cache_expires_at,
                     last_successful_source_run_id,             -- FK → source_runs.id of the run that fetched this offer;
                                                                -- set in the same commit that inserts the offer row (after
                                                                -- external fetch succeeds and run number is allocated);
                                                                -- also serves as audit trail / backfill reference
                     source_successful_run_number_at_fetch,     -- denormalised copy of source_runs.successful_run_number for
                                                                -- that run; set in the same commit alongside the FK above;
                                                                -- avoids a JOIN to source_runs on every optimiser query;
                                                                -- offers are inserted only after a successful fetch, so this
                                                                -- field is always non-NULL on valid offer rows
                     source_confidence_at_ingest)               -- high | medium | low: confidence in the source data itself at
                                                                -- the time of fetch (e.g. BasketWatch = high, Lidl parser = medium)
                                                                -- staleness confidence is separate and computed at query time (see §12)
                     -- append-only: each ingestion run inserts new rows; rows are never updated
                     -- query for "current" offer: SELECT ... ORDER BY fetched_at DESC LIMIT 1 per product
                     -- index: (retailer_product_id, fetched_at DESC)
                     -- retention: keep last 52 weekly snapshots per product; purge older rows periodically

-- Matching
product_matches     (id, household_id, canonical_item_id, retailer_product_id,
                     status,    -- accepted | rejected | suggested | pending
                     matched_by,-- user | system | llm
                     created_at, updated_at)
                     -- UNIQUE(household_id, canonical_item_id, retailer_product_id): one preference row per triple;
                     --   status is updated in place (current-state model, not event-sourced); history is in match_feedback
                     -- household_id explicit: accepted/rejected matches are household preference data
match_feedback      (id, product_match_id, user_id,
                     action,    -- accept | reject | prefer
                     created_at)

-- Ingestion tracking
source_runs         (id, source, started_at, finished_at,
                     requests_made, items_fetched, items_normalised,
                     status, error_message,
                     successful_run_number)   -- monotonic per-source counter, incremented only on status='success'
                                              -- NULL for failed runs; used to count missed cycles since an offer was fetched
                                              -- missed cycles = (latest successful_run_number for source) - offer's run number
                                              -- UNIQUE(source, successful_run_number) WHERE successful_run_number IS NOT NULL
                                              -- ingestion flow (fetch first, short transaction at the end):
                                              --   1. Fetch external data (network I/O — outside any transaction)
                                              --   2. Normalise into in-memory product/offer records
                                              --   3. On success → BEGIN IMMEDIATE:
                                              --        SELECT MAX(successful_run_number) FROM source_runs WHERE source = ?
                                              --        INSERT source_runs (status='success', successful_run_number=MAX+1, ...)
                                              --        INSERT/UPSERT retailer_products and product_offers (with the new run id
                                              --          and run number set on each offer row)
                                              --        COMMIT
                                              --   4. On network/parse failure → short transaction: INSERT source_runs
                                              --        (status='failed', successful_run_number=NULL, error_message=...)
                                              -- never hold a write transaction across network I/O — it blocks all app
                                              -- writes for the duration of BasketWatch/Lidl fetches (potentially minutes)
api_usage_events    (id, source, endpoint, requests_count,
                     month_year,  -- YYYY-MM
                     created_at)

-- Optimisation output
recommendation_runs (id, list_id, household_id,
                     run_at, offer_snapshot_date,
                     plans_json,              -- all four plans serialised
                     recommended_plan,
                     convenience_threshold_2,
                     convenience_threshold_3)
savings_events      (id, list_id, recommendation_run_id,
                     -- all money stored as integer euro cents (×100) to avoid float comparison errors
                     baseline_cost_cents,             -- best single-store cost (the comparison anchor)
                     recommended_cost_cents,          -- cost of the recommended plan
                     theoretical_minimum_cost_cents,  -- absolute cheapest across all stores
                     chosen_basket_cost_cents,        -- actual spend (manual entry after shop)
                     chosen_plan,                     -- single | two_store | three_store | theoretical_min | other
                     chosen_stores,                   -- JSON array of retailer ids actually visited
                     gross_saving_cents,              -- baseline_cost - recommended_cost
                     accepted_savings_cents,          -- saving from recommendations the household followed
                     rejected_savings_cents,          -- saving left on table (followed a pricier plan)
                     travel_cost_cents,               -- manual: extra petrol/time cost for multi-store trip
                     subscription_cost_alloc_cents,   -- pro-rated monthly API subscription cost for this list
                     net_saving_cents,                -- accepted_savings - travel_cost - subscription_cost_alloc
                     created_at)
```

### Key design decisions

- `shopping_lists.current_revision` is a **server-assigned monotonic integer scoped to the list**, incremented on every accepted mutation. `list_entries.last_updated_revision` records which revision last touched that entry. Reconnect replay uses the list-level revision: the client sends its last known revision, the server streams all mutations since that point.
- `mutations` is append-only. The server applies mutations against the client's `base_revision` and assigns `server_revision` = the new `shopping_lists.current_revision`. For 2 users, last-write-wins per field is the default; destructive operations (delete vs concurrent check-off) surface a resolved state and log both events.
- **Mutation retention:** keep the mutation log for **90 days or 10,000 revisions** (whichever is shorter). For a rarely-used second device this is enough headroom without meaningful storage cost on SQLite. If a reconnecting client's `base_revision` is older than the retention window, return a full list snapshot instead of replaying individual mutations. Compact old mutations periodically.
- `product_offers` is append-only. `retailer_products` is the product catalogue. The optimiser always uses the offer row with the most recent `fetched_at` per product — `cache_expires_at` governs when a refresh is due, not whether data is usable. `source_confidence_at_ingest` is stored per row (reflects source reliability at fetch time). **Staleness confidence is computed at query time** as two separate values — `source_confidence` (from `source_confidence_at_ingest`) and `freshness_confidence` (from `current_source_run_number - product_offers.source_successful_run_number_at_fetch`: 0 → high, 1 → low, 2+ → unknown; NULL current run → unknown). The `successful_run_number` field on `source_runs` is a monotonic per-source counter incremented only on success; `source_successful_run_number_at_fetch` on `product_offers` is the denormalised copy set at insert time, making missed-cycle count a simple subtraction without a JOIN.

---

## 8. Bilingual Resolution

Items entered in English or Spanish must resolve to the same `canonical_item`. The cascade produces one of three outcomes: **single match** (proceed), **no match**, or **multiple candidates** (ambiguous).

1. **Alias lookup** — exact normalised match in `item_aliases` → single match, done
2. **Token / fuzzy match** — normalised matching against `canonical_items.name_en`, `canonical_items.name_es`, and all aliases
   - One result above threshold → single match, done; commit to `item_aliases`
   - Zero results → `no_match` outcome (go to step 3)
   - Multiple results above threshold → `multiple_candidates` outcome (go to step 4)
3. **LLM fallback for `no_match`** — cheap LLM API call to find the most plausible canonical item; result committed to `item_aliases` with `source: 'llm'`; if LLM returns no confident match → go to step 4
4. **User clarification** — for `multiple_candidates` (go here directly from step 2) and for unresolved `no_match` (arrive here from step 3); surface choices explicitly (e.g. "cream" → single / double / sour / face / ice cream); confirmed mapping committed to `item_aliases`

`multiple_candidates` goes straight to user clarification, not through the LLM — when the catalogue already contains several plausible matches, the LLM would also be ambiguous and adding a round-trip before the user prompt serves no purpose. The user knows what they want. (See Open Questions §16 if this decision needs revisiting.)

Note: `product_matches` maps `canonical_item → retailer_product` and is not part of this cascade. All confirmed raw-text→canonical mappings are persisted in `item_aliases` (step 1 covers them on next use).

The app must never silently create duplicates or hallucinate a match. Obvious mappings ("leche" → milk) should not generate questions. Confirmed mappings improve all future recognition.

---

## 9. Unit-Price Normalisation

Comparison must use meaningful unit prices, not shelf prices:

| Category | Unit |
|---|---|
| Milk, juice, liquids | €/L |
| Rice, pasta, flour, dry goods | €/kg |
| Nappies | €/nappy |
| Laundry detergent | €/wash |
| Coffee pods | €/pod |
| Toilet/kitchen roll | €/sheet (or €/roll as fallback) |
| Items sold by count | €/each |

BasketWatch provides unit prices; the normaliser validates and supplements them. The pack-size parser handles the Lidl feed which embeds size in the product name.

---

## 10. Optimiser

### Four output plans

For every compiled list, the optimiser generates:

1. **Best single store** — cheapest full-list cost at one retailer, accounting for constraints, matched products, loyalty prices, and basket-threshold vouchers
2. **Best two-store split** — cheapest combination of any two retailers
3. **Absolute theoretical minimum** — every item at its cheapest valid retailer regardless of number of stores (benchmark, not recommendation)
4. **Recommended practical plan** — applies convenience thresholds (see below) to decide whether splitting is worth it

### Convenience threshold

A two-store candidate is recommended only if its saving over the best single-store
option exceeds its applicable second-store threshold. The optimiser evaluates
each candidate separately and selects the best qualifying split; a cheaper
non-qualifying split does not suppress a qualifying alternative:

```
best_single_store_cost - split_cost > applicable_second_store_threshold
```

The household-configurable default second-store threshold is **€10**. If any
store added beyond the best single-store plan is marked nearby, the applicable
threshold is the lower of the configured threshold and **€5**. A nearby store
already in the single-store plan does not lower the bar for adding a distant
store. See [Household store settings](#household-store-settings) for the setting
and API contract.

Other configurable defaults:
- Third store: must save at least **€25–€30** total, or **€20** incremental over the two-store plan

If no two-store candidate clears its applicable threshold, the recommendation
defaults to the best single store.

### Household store settings

Each household stores a disposition per retailer in `household_store_settings`
(migration 011), read and written through `GET`/`PATCH /api/settings`:

- **active** — always eligible for plans (the default for a retailer with no row,
  so a household that configures nothing keeps the pre-existing behaviour);
- **occasional** — eligible only when the household marks a trip to it for the
  current run via the `trip_retailers` field on `POST /api/optimise`;
- **off** — never eligible.

A retailer that is not eligible is dropped before plans are built, so it can
appear in no plan (including the theoretical minimum) and its spend-and-save
voucher hint cannot fire. Marking a store **nearby** lowers the second-store bar
for a split that includes it to `NEARBY_THRESHOLD_2_CENTS` (€5.00), so a close
store such as SuperValu needs a smaller saving to be added than the default €10.

Each store additionally carries **per-category rules** (issue #99) in
`household_store_category_rules` (migration 014): a household may keep a store out
of plans for categories it does not trust it for — the motivating case is Aldi's
fresh food versus its packaged food. A rule names a category, or one sub-category
of it as a refinement, and whether the store may fulfil items in that scope. Rules
are matched most-specific-first (a sub-category rule beats its category rule) and
an item with no matching rule is allowed, so a store with no rules — and an item
the optimiser cannot classify — keeps the pre-existing behaviour. An item is never
assigned to a retailer whose rules exclude it; the recommended plan explains the
skipped store ("skipped for N items by your store category rules"). Rules are
edited on the store settings screen and enforced only in plan generation (see
`docs/KNOWN_LIMITATIONS.md`, "Per-store category rules").

### Retailer incentive modelling

- **Tesco Clubcard** — loyalty price is the active price; assume household has card
- **SuperValu Real Rewards** — same assumption
- **Dunnes spend-and-save** — when Dunnes is eligible for the run (active, or occasional with a trip marked), if its basket is near €50, flag that crossing it may yield a €10 voucher (for a future shop, not the current basket — do not subtract this from current basket total without flagging the timing). Do not show the hint when Dunnes is occasional without a marked trip or is off.
- **Aldi / Lidl offers** — from the cached `offer_text` field; no loyalty programme

### Unresolved and ambiguous entries

List entries with `resolution_status = 'unresolved'` or `'ambiguous'` have no `canonical_item_id` and therefore no `product_matches`. The optimiser **excludes these entries from all priced plan totals**. They are surfaced separately in the recommendation output as "unresolved items" so the household knows the total is a partial estimate. Once the user resolves them (via the clarification flow in §8), the optimiser can be re-run to produce a complete plan.

### Per-store trip view (issue #104)

Choosing a plan from the optimise page allocates it to per-store trips. Each priced
plan line carries the household list entry it fulfils (`entry_id`, echoed by the
optimiser from the basket item) plus the chosen product, so a trip list is a
**view over the shared list entries, never a copy**:

- `/list/<id>/trip` renders one list per store in the chosen plan ("Lidl trip",
  "Tesco order", …), each grouped by that store's category order, showing each
  item's quantity and chosen product, plus an explicit **not allocated** section
  carrying the items the plan could not price (unresolved, no valid offer, or held
  for confirmation) or that are not stocked at its stores. When the plan is chosen,
  the store lists plus that section account for every active list entry, each entry
  once. Entries added after plan selection are absent from both views until the
  shopper chooses a plan again; the trip's stale-plan warning links to the optimiser.
- Shopping mode (`/list/<id>/shopping`) is scoped to the chosen plan: it offers
  only the plan's stores and shows only the selected store's allocated items.
  A link to the whole trip shows how many active items are outside that store's
  allocation, including unallocated items and entries added after plan selection.
  Both the trip page and store-scoped shopping mode show the stale-plan warning
  when an active entry is added, or a planned entry's quantity or canonical item
  changes after the plan is priced. Older cached plans check only snapshot fields
  they contain.
- Check-offs in either view reuse the same optimistic `mark_bought` mutation
  (§11), so an offline check-off queues and drains like any other edit and
  converges across devices.

The chosen allocation is a small client-side cache (per device, `localStorage`),
not server state. Volume one replaces the allocation by re-selecting a plan — there
is no per-line manual reassignment — and three-store routing is a non-goal. When
the list gains an item the plan does not cover, or a planned entry's quantity or
canonical item changes, both trip views show a stale-plan warning linking back to
the optimiser; plan prices are a snapshot and a re-optimise refreshes them.

### Plan explanations

The optimisation screen shows two to four deterministic, plan-specific reasons on
each plan card. Reasons describe the plan's actual lines and may identify cheaper
categories, household brand constraints, coverage and missing prices, price dates
and stale source runs, the convenience-threshold decision, and eligible whole-store
alternatives. A missing qualifying price is presented as missing data, not proof
that a product is unavailable in-store. Unresolved entries remain separately
visible and are excluded from priced totals.

The recommended total is compared with the best single-store total only when both
plans price the same set of items. Otherwise the screen says the totals are not
directly comparable. See the optimisation screen for the rendered explanation;
the response carries ordered reasons per plan in `plan_explanations`.

### Aldi comparison (issue #100)

Aldi prices are collected even when Aldi is switched off or kept out of a
category, so the household can see whether Aldi is worth shopping at — packaged
food today, and fresh food only if the quality concern turns out not to be worth
the price gap. Plans therefore show a side line, "Aldi would have saved €X on
this list", computed against the recommended plan and split into **packaged** and
**fresh** items because the quality concern is about fresh food.

The comparison (`aldi_comparison` on `POST /api/optimise`) is computed *after* the
plans are fixed and independently of them: for every item the recommended plan
prices, the item's cheapest constraint-passing Aldi offer is looked up without
applying the household's store settings or per-store category rules, then totalled
per split. Both sides cover exactly the items Aldi prices *and* the recommended
plan prices, so the figures are like-for-like; an item only one side prices is
excluded from both, and `item_count` reports the covered set. A split with
`item_count == 0` means Aldi has no qualifying price to compare, and the whole
comparison is absent when no plan could be priced. The comparison never changes
which plan is recommended.

"Fresh" is the shared `categories.is_fresh` flag (migration 015): Produce,
Meat & Fish, Dairy and Bakery are fresh; every other category, including ones a
household adds later, is packaged. The split is stored on the basket item by the
API's list → `OptimiserInput` pre-step. The figure is recorded per optimisation
inside `recommendation_runs.plans_json`, but the savings dashboard does not
aggregate it yet (see `docs/KNOWN_LIMITATIONS.md`).

---

### Savings ledger and honest labels (issue #117, roadmap #114)

The ledger must claim only what its evidence proves. Every `savings_events` row
carries a `status` (migration 016) and the dashboard labels each figure with its
baseline, evidence coverage, date, and included costs:

* **`verified`** — line- and quantity-reconciled evidence. **Reserved**: no
  production path writes it yet (it needs the purchase ledger of #116), so the
  aggregate the API calls `saved` is empty today and shows an honest empty state.
  Only `verified` rows may ever be counted as saved or net savings.
* **`estimated`** — the best single store and the recommended plan price the same
  lines, so `baseline − recommended` is a like-for-like *opportunity estimate*,
  not a realised saving.
* **`unreconciled`** — the two plans price different lines. The stored gross
  figure is kept for audit but never presented or aggregated as a saving. This is
  the coverage guard at archive time, reusing the same line-set comparison the
  optimise explanation uses to refuse a cross-plan cost comparison
  (`optimiser::prices_same_lines`).
* **`unchecked`** — a pre-guard row whose comparability was never checked. No
  saving is claimed and it is excluded from both aggregates.
* **`legacy_total_only`** — a manual whole-basket actual-spend total compared
  against the best single store estimate. Quantities and purchases are
  unverified; travel is included where entered; service and data costs are
  excluded. It is shown separately as a legacy basket variance and is never saved
  money. Realised figures group by the list's archive date
  (`shopping_lists.archived_at`), not the row's creation timestamp.

**Accepted rules (household owner, from the #117 methodology report).** Every
displayed saving names its baseline, evidence coverage, date and included costs.
Partial or unreconciled results are labelled and never aggregated as verified
savings. Old total-only events become a clearly labelled legacy variance, kept
for audit and never relabelled as verified. Buying fewer items must never create
a claimed optimisation saving. The fixture report's theoretical €77.96/month
(`docs/phase0-results.md`) and the recommended plan's approximately €45.59/month
before costs must never be described as practical live savings.

**Decisions.**

1. **Usual-shop baseline.** The household's written "usual shop" profile is the
   primary baseline for the live evaluation (#109); the best-single-store estimate
   remains the anchor for the stored recommendation comparison. No new baseline is
   implemented in this slice.
2. **Strict pack-size rule.** Only verified equivalent units up to the planned
   amount count as matching the plan; extra stock (buying a larger pack, or more
   units than planned) is shown separately and never claimed as saving.
3. **Legacy total-only label.** Existing total-only events are relabelled
   `legacy_total_only` and presented under their own heading with their original
   figures, rather than being migrated or silently re-presented as verified.

---

## 11. Sync Protocol

The client maintains a local Dexie database mirroring the server's list state and an append-only mutation queue.

### Mutation schema (client → server)

```json
{
  "client_mutation_id": "uuid-v4",
  "device_id": "stable-device-uuid",
  "user_id": "user-uuid",
  "list_id": "list-uuid",
  "base_revision": 47,
  "entity_type": "list_entry",
  "entity_id": "entry-uuid",
  "operation_type": "mark_bought",
  "payload": { "bought_at": "2026-05-28T14:32:00Z" },
  "created_at_client": "2026-05-28T14:32:00.123Z"
}
```

### Server response

```json
{
  "client_mutation_id": "uuid-v4",
  "status": "accepted",
  "server_revision": 48,
  "entity": { ... }
}
```

### Conflict rules

| Conflict | Resolution |
|---|---|
| Same field updated by two clients | Last server revision wins (last-write-wins per field) |
| Item deleted by one, checked off by other | Keep deleted; surface notification |
| Duplicate add — same device retry | Deduplicated by `client_mutation_id` (idempotency key) |
| Duplicate add — different devices, same item | Canonical deduplication: partial unique index on `(list_id, canonical_item_id, effective_constraint_hash)` where `status = 'active' AND canonical_item_id IS NOT NULL`. Same hash → merge (increment `quantity`). Different hash → separate entry ("milk" and "lactose-free milk" share a canonical item but differ on `effective_constraint_hash` → two rows). Unresolved entries are excluded by the predicate and are never merged. |
| Quantity changed by two clients (`set_quantity`) | Last server revision wins. `set_quantity` carries an **absolute** value (`payload.quantity`, int in `1..=9999`) so retries/LWW are idempotent — a replayed or stale-queued op can never double-count (unlike a delta). The server rejects a quantity outside `1..=9999` (422). This is the syncable form of the list UI's +/- controls (the `-` button is disabled at 1 and `+` at 9999). |

SSE stream pushes `{ event: "mutation", server_revision: N, entity: {...} }` to all connected clients for the same list. On reconnect, client sends its last known `list_revision`; server streams all mutations since that revision, or returns a full snapshot if the client is beyond the retention window (see mutation retention in §7).

---

## 12. Ingestion Adapters

### BasketWatch via RapidAPI

- **Plan:** Pro — $9.99/month, 5,000 requests/month, 60 RPM
- **Coverage:** Tesco, Dunnes Stores, SuperValu, Aldi
- **Cadence:** Weekly refresh (Friday early AM), aligned to BasketWatch's own update schedule
- **Self-imposed rate cap:** 40 RPM (buffer below the 60 RPM limit)
- **Normalisation:** Raw JSON is parsed and discarded immediately; only compact `retailer_products` + `product_offers` rows are persisted

### Lidl direct

- **Source:** Lidl.ie product search
- **Confidence:** Initially medium (treat as lower confidence until validated)
- **Cadence:** Weekly, same as BasketWatch
- **Isolation:** Lidl adapter failure must not affect BasketWatch pipeline

### API usage guardrails

| Threshold | Action |
|---|---|
| 3,500 requests/month | Warn in admin UI |
| 4,500 requests/month | Disable non-essential refreshes (skip re-fetching already-current items) |
| 4,900 requests/month | Hard stop all external calls |

Tracked in `api_usage_events` per source, per month.

### Stale data fallback

If a weekly refresh fails or returns an empty payload, the previous cycle's data is retained rather than dropped. The optimiser always uses the offer row with the most recent `fetched_at` per product regardless of `cache_expires_at`.

Each offer row has `source_confidence_at_ingest` (stored at fetch time; e.g. BasketWatch = high, Lidl parser = medium). **Freshness confidence is computed at query time** using the denormalised `source_successful_run_number_at_fetch` field: `missed_cycles = current_source_run_number - offer.source_successful_run_number_at_fetch`. Zero missed = `high`; one = `low`; two or more = `unknown`. Fallback: if no successful source run exists yet for that source (e.g. Phase 0 fixture data loaded before any live ingestion run), `current_source_run_number` is NULL — treat freshness_confidence as `unknown` in that case rather than erroring. The recommendation output exposes both separately (`source_confidence` and `freshness_confidence`) rather than one overloaded field. No update-in-place needed on the append-only table.

---

## 13. Offline Behaviour

The shopping list must remain fully usable offline (read and write). The optimiser can require connectivity (it reads from server-side cached data; no live API calls).

Offline capabilities:
- View active list
- Check off items
- Mark skipped
- Add new items (queued as mutations)
- Paste a multi-line list to preview, edit and add several items at once (queued as mutations)
- Add quick notes
- View the chosen plan's per-store trip lists and check items off within a store
  (issue #104) — the allocation is a cached view over the same entries, so it
  needs no network

Service worker caches the SvelteKit app shell. Dexie holds the local list mirror and mutation queue. On reconnect, the mutation queue drains against the server in order.

The list and shopping-mode screens show an always-visible sync status: synced, syncing, offline with the number of pending changes, or a sync error for rejected changes. Each item with a pending or sent mutation has a subtle marker until the server confirms it. Rejected mutations remain visible as an error until the user dismisses them; dismissal reconciles local state from the server snapshot before clearing the error.

---

## 14. Build Order

Build in this sequence to validate assumptions before investing in the polished app:

### Phase 0 — Validation (CLI only)
- Rust project scaffold, Cargo workspace
- **Local-only dev mode:** feature flag (env var or compile flag) that disables all external API calls and loads offer data from fixture files — no BasketWatch key required to develop or test
- SQLite schema (core tables: `canonical_items`, `item_aliases`, `item_constraints`, `categories`, `retailer_products`, `product_offers`, `source_runs`, `product_matches`) — `source_runs` required because `product_offers.last_successful_source_run_id` is a FK to it; `product_matches` required because the optimiser filters retailer candidates through it
- Phase 0 `product_matches` are **fixture-driven**: a JSON fixture file pre-defines `canonical_item → retailer_product` mappings for the validation basket; no UI matching flow yet
- Seed data from Shoppy reference: 11 categories, 226 bilingual dictionary entries as `canonical_items` + `item_aliases`
- BasketWatch adapter (fetch + normalise to SQLite); guarded by local-only flag
- Lidl adapter (initial version, even if partial)
- Unit-price normaliser (port Shoppy's accent-stripping, quantity-parsing logic to Rust)
- Optimiser: single-store and two-store plans
- Convenience threshold logic
- CLI: `grocery validate-basket fixtures/items.json`, `grocery optimise --list fixtures/weekly-shop.json`, `grocery ingest-basketwatch --dry-run`

**Gate:** run against a reconstructed real weekly shop. If realistic monthly savings < €50, pause. If €100+, proceed.

### Phase 1 — Backend API
- Remaining SQLite tables (lists, mutations, users, households, sessions, `store_category_orders`)
- Default `store_category_orders` rows seeded for all 5 retailers × 11 categories
- Axum API: auth endpoints, list CRUD, mutation endpoint, SSE stream
- Litestream sidecar config → Cloudflare R2
- Hetzner deployment (systemd service, binary + DB volume)

### Phase 2 — PWA (implemented)
- SvelteKit scaffold
- Dexie schema (local list mirror + mutation queue)
- Service worker (app shell + offline)
- Sync loop (drain mutation queue on reconnect; apply SSE updates)
- Basic list UI: add item, check off, mark skipped
- **Duplicate warning UX** (ported from Shoppy): when a new item matches an already-active entry, show merge-quantity / add-anyway choice — do not silently deduplicate. Phase 2 uses **dictionary-only local detection** (bundled bilingual dictionary in Dexie, client-side). Matches are used for the in-session warning UI only — no aliases are written to `item_aliases` in Phase 2. The full server-side resolution cascade (§8) is built in Phase 3, at which point confirmed mappings begin persisting to `item_aliases` and the duplicate check is upgraded automatically
- **Shopping mode**: store-selector switches to store-specific category ordering from `store_category_orders`; items grouped by aisle; progress bar; this view works usefully even when price data is stale. With a chosen plan, it is scoped to that plan's stores and allocations (§10).

### Phase 3 — Bilingual matching
- Alias lookup and resolution cascade in the API (§8)
- LLM fallback for unresolved `no_match` terms (not for `multiple_candidates` — those go straight to user clarification)
- Ambiguity confirmation flow in the PWA
- `item_aliases` feedback loop (confirmed mappings persist)

### Phase 4 — Optimiser UI (implemented)
- Optimisation endpoint in Axum
- PWA: basket plan display (4 plans, plain-language recommendation) and per-store trip view (§10)
- Convenience threshold configuration

### Phase 5 — Household learning & savings ledger
- `match_feedback` and `product_matches` persistence
- Rejected suggestion suppression
- `savings_events` logging on list archive
- Basic savings summary view

---

## 15. Borrowed from Shoppy

An earlier prototype (`~/projects/shoppy`) used Next.js + Supabase. That stack is not continued, but the following assets are reused:

| Asset | Location in Shoppy | How it's used in new design |
|---|---|---|
| 226-entry bilingual dictionary | `src/lib/matching/dictionary.ts` | Phase 0 seed for `canonical_items` + `item_aliases` (EN/ES terms, canonical form, category, variants) |
| 11 grocery categories | `supabase/migrations/002_seed_categories.sql` | Seed for `categories` table; fix accent inconsistencies on import |
| Text normalisation rules | `src/lib/matching/normalize.ts` | Port to Rust: lowercase, NFD accent stripping, punctuation → space (not deletion — `"coca-cola"` → `"coca cola"`, not `"cocacola"`; exception: known brand compaction rules), whitespace collapse |
| Quantity + unit parser | `src/lib/matching/normalize.ts` → `parseQuantityAndUnit` | Port to Rust and **extend**: handles `"2x"`, `"500g"`, `"dos"` (word numbers EN/ES); extend to cover Spanish long-form units (e.g. `"dos litros"` — `"litros"` is not in Shoppy's current unit list) |
| Fuzzy matching thresholds | `src/lib/matching/fuzzy.ts` | Reference for Rust `strsim` tuning: Levenshtein threshold=2, minLength=4, bidirectional contains |
| Duplicate detection cascade | `src/lib/matching/DuplicateDetector.ts` | Reference spec: exact → dictionary → fuzzy → contains, confidence scores |
| Duplicate warning UX | `src/components/list/DuplicateWarning.tsx` | Port interaction pattern to Svelte: merge-quantity / add-anyway choice |
| Store-specific category ordering | `store_profiles.category_order` + `CategoryOrderList.tsx` | Drives `store_category_orders` table and shopping-mode aisle grouping |
| Shopping mode | `src/app/(app)/list/shopping/page.tsx` | Reference for store-ordered category groups + progress bar in SvelteKit |
| Sync queue with retry | `src/lib/sync/SyncQueue.ts` | Reference pattern for Dexie mutation queue: MAX_RETRIES=3, retry-on-reconnect |
| Matching test fixtures | `src/lib/matching/*.test.ts` | Port test cases to Rust unit tests for the bilingual resolution cascade |

**Not reused:** Supabase schema/RLS/auth, Next.js app structure, Supabase Realtime sync, OAuth/magic-link, timestamp-based conflict model, Vercel deployment, public invite flow.

---

## 16. Open Questions

To resolve before or during Phase 0:

1. **BasketWatch API shape** — does it support product-by-ID lookup? Is search per-retailer or cross-retailer? What is the exact weekly refresh schedule?
2. **Lidl.ie reliability** — how much product data can be reliably extracted, and are prices current enough to trust?
3. **Dunnes voucher mechanics** — current thresholds and whether vouchers apply to the current basket or a future shop (do not model as current-basket discount without verifying)
4. **App name** — working name "grocery-optimizer"; shortlist leans toward Spanish-flavoured optimiser names (e.g. Listo, Ahorra, Neto)
5. **Savings baseline** — compare savings against the household's usual shop, or against the best single-store basket? (Both have merit; best-single-store baseline is less gameable)
6. **`multiple_candidates` routing** — §8 resolves that `multiple_candidates` goes straight to user clarification, skipping the LLM. Revisit if UX testing shows users are annoyed by frequent clarification prompts on items the LLM could confidently resolve.

---

## 17. Non-Goals

- Public or multi-tenant SaaS
- Native mobile app (PWA is sufficient)
- Receipt OCR scanning
- Meal planning or nutrition tracking
- Online ordering or checkout integration
- Daily full-catalogue historical snapshots
- Full price history analytics (savings ledger is enough)
- Multi-region high availability (Litestream + restore is sufficient for a household app)
