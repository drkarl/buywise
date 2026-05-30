# Phase 0 — Validation Results

Phase 0 builds a CLI optimiser over fixture and live SQLite data to validate the
core BuyWise logic end-to-end (resolution → matching → unit-price normalisation
→ optimisation) and to check the **gate**: a realistic monthly grocery saving of
**≥ €50** (design §1 / `implementation-plan.md` P0-T11).

## Gate result

| Metric | Value |
|---|---|
| Best single store (Tesco — stocks the whole basket) | €105.80 / week |
| Best two-store split (Aldi + Tesco) | €95.28 / week |
| Theoretical minimum (all stores) | €87.81 / week |
| **Achievable weekly saving** (best single − theoretical min) | **€17.99 / week** |
| **Estimated monthly saving** (×52/12) | **€77.96 / month** |
| Recommended plan | Two-store (Aldi + Tesco), €95.28 (saves €10.52/wk, clears threshold) |

**€77.96 / month ≥ €50 → Phase 0 gate PASSED.** Proceed to Phase 1.

> **These figures are illustrative, not a real-data gate result.** They are
> computed from the hand-authored prices in `fixtures/offers.json` (see
> [Caveat on the prices](#caveat-on-the-prices)); €77.96 demonstrates the
> pipeline clearing the €50 gate on representative data. The real-price gate is
> re-run in live mode before Phase 1 ships.

The achievable saving is the gate metric: the most a household saves by buying
each item at its globally cheapest store versus shopping at the single best
store. The *recommended* plan applies the second-store convenience threshold
(€10/week), so it captures €10.52/week (€45.59/month) of that saving with only
one extra stop.

## How the figure was produced

The same offer snapshot drives both modes, so the printed saving is identical;
live mode additionally exercises the real ingest + seed-matches pipeline.

### Fixture mode (CI smoke test — illustrative prices)

```
buywise-cli validate-basket fixtures/items.json
# → resolved 30/30 (100%)

buywise-cli optimise --list fixtures/weekly-shop.json
# → four plans; estimated monthly saving €77.96
```

### Live mode (the gate path)

```
buywise-cli ingest-basketwatch --db buywise.db --dry-run
# → dry-run mode: reads fixtures/offers.json, writes 73 products + 73 offers,
#   0 network requests (1 record skipped: null external_id)

buywise-cli seed-matches \
    --from fixtures/product_matches.csv \
    --db buywise.db \
    --household-id 00000000-0000-0000-0000-000000000001
# → 73 product_matches upserted (idempotent: a second run leaves 73, not 146)

buywise-cli optimise --list fixtures/weekly-shop.json --db buywise.db
# → estimated monthly saving €77.96 (matches fixture mode)
```

## Plan invariant

Across the complete plans the documented ordering holds:

```
theoretical_minimum (€87.81)  ≤  best_two_store (€95.28)  ≤  best_single (€105.80)
missing_count: 0                  0                            0
```

`best_two_store` spans exactly two distinct stores (Aldi + Tesco), and every
plan is complete (`missing_count == 0`). Tesco stocks the whole basket, so the
best single-store plan is always complete; cheaper Aldi/Dunnes/SuperValu offers
keep the two-store split and the theoretical minimum strictly below it.

## Note on the brand-constrained item

`fixtures/item_constraints.json` carries a Fairy brand + 500 ml size constraint
for the `dish soap` canonical item. The dictionary keeps that item's
`default_constraint_policy = 'any'` on purpose: live mode (`ingest-basketwatch` +
`seed-matches`) does **not** seed `item_constraints` rows, and a `brand` policy
with no constraints row would (correctly) send the item to
`no_valid_offer_items`, making live and fixture modes diverge. This is exactly
the caveat in `implementation-plan.md` P0-T11 ("do not rely on product matches
alone for any item whose dictionary policy is `brand`"). Following that doc's
option (a), Phase-0 keeps brand items as `any` and offers only constraint-valid
products (dish soap is sold only as Fairy 500 ml here). The size band (±20 %)
still applies in fixture mode via the seeded constraints row. Hard brand
enforcement is exercised by the optimiser's own unit tests
(`crates/core/src/optimiser/tests.rs`); a `seed-constraints` command (the doc's
option (b)) is deferred to when live brand items are needed.

## Note on nappies

`nappies_size_4` and `nappies_size_5` are separate canonical items (Phase-0
scope: nappy size is distinct slugs, never one `nappies` item with a runtime
size constraint). They are present in `fixtures/offers.json` and
`fixtures/product_matches.csv` so the data layer carries them, but they are not
placed on a `weekly-shop` basket line: "Nappies Size 4" and "Nappies Size 5"
are within fuzzy edit-distance of each other and resolve *ambiguously*, so a
basket line for them would never price. The separation requirement is satisfied
structurally by the canonical items + offers; basket resolution of nappy sizes
is a Phase-3 clarification-flow concern.

## Caveat on the prices

Phase-0 fixture prices in `fixtures/offers.json` are **illustrative**,
hand-constructed so that no single store wins every item (Aldi is cheapest on
most staples while Tesco/SuperValu/Dunnes win several via loyalty or category
strength; Tesco is the baseline that stocks everything). They demonstrate the
optimiser surfacing a ≥ €50/month opportunity; they are not live market data.
The real gate is re-run against live BasketWatch prices before Phase 1 ships
(`implementation-plan.md` P0-T11, mode 2), using `fixtures/product_matches_live.csv`
mapped by hand after a live ingest.

## Tooling

`fixtures/offers.json`, `fixtures/product_matches.csv`,
`fixtures/weekly-shop.json`, and `fixtures/items.json` are regenerated
deterministically by `scripts/gen_phase0_fixtures.py` against the committed
`fixtures/canonical_items.json` (so every basket line is genuinely priceable and
every CSV row references a real `retailer_product_id`). `canonical_items.json`
and `migrations/002_seed.sql` are themselves generated from
`fixtures/dictionary.json` via `buywise-cli generate-fixtures`.
