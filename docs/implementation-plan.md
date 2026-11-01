# BuyWise — Implementation Plan

**Based on:** `docs/design.md` v13  
**Repository:** Rust workspace (`crates/`) with a SvelteKit PWA in `frontend/`  
**Date:** 2026-05-28

---

## Overview

BuyWise is built in six phases. Each phase has a concrete gate — do not begin the next phase until the gate passes.

The **critical path** is: Phase 0 (validate savings ≥ €50/month) → Phase 1 (persistent API) → Phase 2 (PWA + offline) → Phases 3–5 (progressively smarter). The first three phases constitute the minimum useful product. Phases 4–5 are meaningful only if Phase 0 confirms the savings case.

**AI is optional, not a milestone.** No phase requires an AI provider or a model download. The LLM fallback (P3-T2) and the embedding shortlist are off by default, are not gate criteria for any phase, and the product described above works fully without either. The plan keeps them where they sit rather than treating them as committed requirements.

**Architectural principles enforced throughout:**

- Money is always stored as integer cents (or micros for unit prices). No floats.
- `product_offers` is append-only. Never UPDATE a row; always INSERT a new one.
- `mutations` is append-only. The server applies and assigns `server_revision` atomically.
- Fetch external data outside any transaction. Begin a `BEGIN IMMEDIATE` only after network I/O succeeds.
- All CHECK constraints on `list_entries` must be defined in the initial CREATE TABLE — SQLite cannot add them via ALTER TABLE.
- The partial unique index on `list_entries (list_id, canonical_item_id, effective_constraint_hash) WHERE status = 'active' AND canonical_item_id IS NOT NULL` is the dedup boundary; code that inserts/updates entries must respect it.
- `unresolved:<entry_id>` and `'default'` are sentinel values for `effective_constraint_hash`, not real hashes. Both are valid NOT NULL values that satisfy the uniqueness rule while the entry has no constraints.
- `source_successful_run_number_at_fetch` on `product_offers` is a denormalised copy — set once at insert time alongside the FK. It is never updated. Freshness confidence is computed at query time via subtraction.

**Workspace structure (current scaffold):**

```
Cargo.toml              workspace manifest
crates/
  core/                 domain model, optimiser, matching, normalise (all stubs)
  api/                  Axum server stub
  ingest/               ingestion adapters stub
  cli/                  CLI stub
frontend/               not yet scaffolded
migrations/             not yet created
fixtures/               not yet created
deploy/
  litestream.yml        R2 config (endpoint placeholder)
docs/
  design.md
```

All `src/` files are one-line stubs. Nothing is implemented yet.

---

## Dependency Graph

```
Phase 0 tasks (P0):
  P0-T1  workspace deps + Cargo.toml
    └─► P0-T2  SQLite migrations (Phase 0 tables)
          └─► P0-T3  seed data (categories + canonical_items)
          └─► P0-T4  domain types (crates/core/src/domain.rs)
                └─► P0-T5  text normaliser (crates/core/src/normalise.rs)
                      └─► P0-T6  bilingual matching – local only (crates/core/src/matching.rs)
                └─► P0-T7  unit-price normaliser (extend normalise.rs)
                └─► P0-T8  BasketWatch ingest adapter (crates/ingest)
                      └─► P0-T9  Lidl direct adapter (crates/ingest)
                └─► P0-T10 optimiser: single-store + two-store plans (crates/core/src/optimiser.rs)
                      └─► P0-T11 CLI commands (crates/cli)
                            └─► Phase 0 gate: validate savings ≥ €50/month

Phase 1 tasks (P1):
  P0 complete
    └─► P1-T1  remaining migrations (lists, mutations, sessions, users, households)
          └─► P1-T2  auth endpoints (login, logout, session check)
          └─► P1-T3  list CRUD endpoints
          └─► P1-T4  mutation endpoint (POST /api/mutations)
                └─► P1-T5  SSE stream (GET /api/sse/:list_id)
          └─► P1-T6  Axum server wiring + static SvelteKit serving
          └─► P1-T7  Hetzner deployment (systemd + Litestream)

Phase 2 tasks (P2):
  P1 complete
    └─► P2-T1  SvelteKit scaffold + Dexie schema
          └─► P2-T2  service worker (app shell offline)
          └─► P2-T3  list UI (add, check off, skip, delete)
                └─► P2-T4  sync loop (drain queue, apply SSE)
          └─► P2-T5  duplicate warning UX (client-side dictionary only)
          └─► P2-T6  shopping mode (store selector, aisle groups, progress)

Phase 3 tasks (P3):
  P2 complete
    └─► P3-T1  server-side resolution cascade (alias → token/fuzzy → optional LLM → user)
          └─► P3-T2  LLM fallback integration (no_match only) — optional, off by default
          └─► P3-T3  ambiguity confirmation flow in PWA
          └─► P3-T4  item_aliases feedback persistence

Phase 4 tasks (P4):
  P3 complete
    └─► P4-T1  POST /api/optimise endpoint (four plans)
          └─► P4-T2  convenience threshold configuration endpoint (complete; `GET`/`PATCH /api/settings`)
          └─► P4-T3  basket plan display in PWA (four plans + plain-language recommendation)

Phase 5 tasks (P5):
  P4 complete
    └─► P5-T1  match_feedback + product_matches write paths
    └─► P5-T2  rejected suggestion suppression in optimiser
    └─► P5-T3  savings_events logging on list archive
    └─► P5-T4  savings summary view in PWA
```

---

## Phase 0 — CLI Validation

**Gate:** Two modes — (1) **fixture smoke test** (`buywise-cli optimise --list fixtures/weekly-shop.json`, no `--db`): validates the pipeline locally, runs in CI, but uses static fixture prices so the saving estimate is illustrative only. (2) **Live validation**: `ingest-basketwatch --db buywise.db` → `seed-matches --from fixtures/product_matches_live.csv --db buywise.db --household-id 00000000-0000-0000-0000-000000000001` → `optimise --list fixtures/weekly-shop.json --db buywise.db`; uses real prices from SQLite. The gate value comes from mode 2. If realistic monthly savings < €50, stop. If ≥ €50, continue to Phase 1.

---

### P0-T1: Workspace dependencies

**Type:** chore

**Description:**

Add all third-party dependencies to each crate's `Cargo.toml`. Nothing compiles correctly until these are present. The workspace root does not need `[workspace.dependencies]` at this stage; per-crate is fine.

Required per crate:

- `crates/core`: `sqlx` (features: `sqlite`, `runtime-tokio`, `macros`), `serde` (features: `derive`), `serde_json`, `uuid` (features: `v4`, `serde`), `strsim`, `unicode-normalization`, `sha2`, `hex`, `thiserror`, `chrono` (features: `serde`), `tokio` (features: `full`, dev-dependency only for tests)
- `crates/ingest`: inherits `core`; add `reqwest` (features: `json`, `rustls-tls`), `tokio` (features: `full`), `serde`, `serde_json`, `anyhow`, `tracing`, `tracing-subscriber`
- `crates/cli`: inherits `core`, `ingest`; add `clap` (features: `derive`), `tokio` (features: `full`), `anyhow`, `tracing`, `tracing-subscriber`, `serde_json`
- `crates/api`: inherits `core`, `ingest`; add `axum` (features: `ws` not needed; use default), `tokio` (features: `full`), `tower-http` (features: `fs`, `cors`), `sqlx`, `serde`, `serde_json`, `uuid`, `anyhow`, `tracing`, `tracing-subscriber`, `argon2`, `rand`

**Files / Areas Touched:**
- `crates/core/Cargo.toml`
- `crates/ingest/Cargo.toml`
- `crates/cli/Cargo.toml`
- `crates/api/Cargo.toml`

**Acceptance Criteria:**
- [ ] `cargo build --workspace` succeeds with no errors
- [ ] `cargo test --workspace` compiles (zero tests pass; that is fine at this point)
- [ ] No `unused import` warnings that indicate a version mismatch

**Testing Expectations:**
- Compilation only; no logic tests at this task

---

### P0-T2: SQLite migrations — Phase 0 tables

**Type:** chore

**Description:**

Create the `migrations/` directory and write the initial migration files using sqlx's numbered convention (`001_initial.sql`, `002_seed.sql`, etc.). Phase 0 needs the following tables only:

```
categories
canonical_items
item_aliases
item_constraints
retailer_products
product_offers
source_runs
product_matches
```

**`api_usage_events` must also be created in Phase 0** — the BasketWatch adapter (P0-T8) reads and writes it before every request batch to enforce quota guardrails. Including it in Phase 1 would make live API validation in Phase 0 impossible.

The remaining tables (`shopping_lists`, `list_entries`, `mutations`, `users`, `households`, `household_members`, `sessions`, `list_entry_events`, `store_category_orders`, `recommendation_runs`, `savings_events`, `match_feedback`) are deferred to Phase 1 (P1-T1). Do not define them here; sqlx migrations are applied in order and Phase 1 will append them cleanly.

**Schema details** (see design §7, replicated here for completeness):

```sql
-- 001_phase0_schema.sql

CREATE TABLE categories (
    id          TEXT PRIMARY KEY,          -- fixed UUID (see seed)
    name_en     TEXT NOT NULL,
    name_es     TEXT NOT NULL,
    icon        TEXT NOT NULL,
    default_order INTEGER NOT NULL
);

CREATE TABLE canonical_items (
    id                      TEXT PRIMARY KEY,
    household_id            TEXT NOT NULL,
    name_en                 TEXT NOT NULL,
    name_es                 TEXT NOT NULL,
    category_id             TEXT NOT NULL REFERENCES categories(id),
    default_constraint_policy TEXT NOT NULL
        CHECK(default_constraint_policy IN
              ('any','brand','allowed_set','own_label','exact','ask')),
    created_at              TEXT NOT NULL  -- ISO-8601
);

CREATE TABLE item_aliases (
    id               TEXT PRIMARY KEY,
    household_id     TEXT NOT NULL,
    canonical_item_id TEXT NOT NULL REFERENCES canonical_items(id),
    alias            TEXT NOT NULL,
    language         TEXT NOT NULL CHECK(language IN ('en','es')),
    source           TEXT NOT NULL CHECK(source IN ('manual','accepted_match','llm')),
    created_at       TEXT NOT NULL
);
CREATE UNIQUE INDEX uq_item_aliases_household_alias_lang
    ON item_aliases(household_id, lower(alias), language);

CREATE TABLE item_constraints (
    id                      TEXT PRIMARY KEY,
    canonical_item_id       TEXT NOT NULL UNIQUE REFERENCES canonical_items(id),
    brand                   TEXT,
    allowed_brands          TEXT,          -- JSON array or NULL
    excluded_brands         TEXT,          -- JSON array or NULL
    -- Size in canonical base units (g / ml / each)
    preferred_size_value    REAL,
    preferred_size_basis    TEXT CHECK(preferred_size_basis IN ('g','ml','each') OR preferred_size_basis IS NULL),
    min_size_value          REAL,
    min_size_basis          TEXT CHECK(min_size_basis IN ('g','ml','each') OR min_size_basis IS NULL),
    max_size_value          REAL,
    max_size_basis          TEXT CHECK(max_size_basis IN ('g','ml','each') OR max_size_basis IS NULL),
    pack_count              INTEGER,
    form_factor             TEXT,
    dietary_flags           TEXT,          -- JSON array or NULL
    substitution_policy     TEXT NOT NULL DEFAULT 'any'
        CHECK(substitution_policy IN ('any','ask','strict')),
    updated_at              TEXT NOT NULL
);

CREATE TABLE source_runs (
    id                    TEXT PRIMARY KEY,
    source                TEXT NOT NULL CHECK(source IN ('basketwatch','lidl_direct','manual')),
    started_at            TEXT NOT NULL,
    finished_at           TEXT,
    requests_made         INTEGER NOT NULL DEFAULT 0,
    items_fetched         INTEGER NOT NULL DEFAULT 0,
    items_normalised      INTEGER NOT NULL DEFAULT 0,
    status                TEXT NOT NULL CHECK(status IN ('running','success','failed')),
    error_message         TEXT,
    successful_run_number INTEGER            -- NULL for failed runs
);
CREATE UNIQUE INDEX uq_source_runs_source_run_number
    ON source_runs(source, successful_run_number)
    WHERE successful_run_number IS NOT NULL;

CREATE TABLE retailer_products (
    id              TEXT PRIMARY KEY,
    source          TEXT NOT NULL CHECK(source IN ('basketwatch','lidl_direct','manual')),
    retailer        TEXT NOT NULL CHECK(retailer IN ('tesco','dunnes','supervalu','aldi','lidl')),
    external_id     TEXT,                  -- NULL for manual
    url             TEXT,
    name            TEXT NOT NULL,
    brand           TEXT,
    category        TEXT,
    pack_size_text  TEXT,
    size_value      REAL,
    size_unit       TEXT,
    pack_count      INTEGER,
    first_seen_at   TEXT NOT NULL,
    last_seen_at    TEXT NOT NULL
);
-- Partial unique index for API-sourced products
CREATE UNIQUE INDEX uq_retailer_products_source_external
    ON retailer_products(source, retailer, external_id)
    WHERE source != 'manual' AND external_id IS NOT NULL;

CREATE TABLE product_offers (
    id                                    TEXT PRIMARY KEY,
    retailer_product_id                   TEXT NOT NULL REFERENCES retailer_products(id),
    price_cents                           INTEGER NOT NULL,     -- euros × 100 (integer cents); no floats
    unit_price_eur_micros                 INTEGER,              -- €/unit × 1_000_000
    unit_price_basis                      TEXT,                 -- per_kg | per_l | per_each | per_sheet | ...
    previous_price_cents                  INTEGER,
    loyalty_price_cents                   INTEGER,
    loyalty_scheme                        TEXT CHECK(loyalty_scheme IN ('clubcard','real_rewards') OR loyalty_scheme IS NULL),
    offer_text                            TEXT,
    offer_valid_until                     TEXT,
    source_snapshot_date                  TEXT,
    fetched_at                            TEXT NOT NULL,
    cache_expires_at                      TEXT,
    last_successful_source_run_id         TEXT REFERENCES source_runs(id),
    source_successful_run_number_at_fetch INTEGER,              -- denormalised; never updated
    source_confidence_at_ingest           TEXT NOT NULL
        CHECK(source_confidence_at_ingest IN ('high','medium','low'))
);
-- Query pattern: latest offer per product
CREATE INDEX idx_product_offers_product_fetched
    ON product_offers(retailer_product_id, fetched_at DESC);

CREATE TABLE product_matches (
    id                  TEXT PRIMARY KEY,
    household_id        TEXT NOT NULL,
    canonical_item_id   TEXT NOT NULL REFERENCES canonical_items(id),
    retailer_product_id TEXT NOT NULL REFERENCES retailer_products(id),
    status              TEXT NOT NULL CHECK(status IN ('accepted','rejected','suggested','pending')),
    matched_by          TEXT NOT NULL CHECK(matched_by IN ('user','system','llm')),
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
CREATE UNIQUE INDEX uq_product_matches_household_item_product
    ON product_matches(household_id, canonical_item_id, retailer_product_id);

-- api_usage_events included in Phase 0 because the BasketWatch adapter
-- reads/writes it before each request batch to enforce quota guardrails.
CREATE TABLE api_usage_events (
    id             TEXT PRIMARY KEY,
    source         TEXT NOT NULL,
    endpoint       TEXT NOT NULL,
    requests_count INTEGER NOT NULL,
    month_year     TEXT NOT NULL,   -- YYYY-MM
    created_at     TEXT NOT NULL
);
```

Write the SQL into `migrations/001_phase0_schema.sql`. Run it via sqlx's CLI (`sqlx migrate run`) or in-process via `sqlx::migrate!()`.

**JSON1 dependency:** the `list_entries` CHECK constraint `json_array_length(resolution_candidates_json) > 0` requires SQLite's JSON1 extension. SQLite ships with JSON1 enabled by default since 3.38 (2022-02-22), but verify this at startup and in CI. With sqlx, use the `bundled` feature (`sqlx = { features = ["sqlite", "bundled"] }`) to pin a known-good SQLite version and avoid environment differences. Add a startup assertion: `SELECT json_array_length('[]')` — if this errors, abort with a clear message.

**Files / Areas Touched:**
- `migrations/001_phase0_schema.sql` (create)
- `crates/core/src/db.rs` (create: connection setup, `sqlx::migrate!()` call, JSON1 startup check)
- `crates/core/src/lib.rs` (expose `db` module)

**Acceptance Criteria:**
- [ ] `sqlx migrate run --database-url sqlite://buywise.db` applies without error on a fresh file
- [ ] Running migrate again is idempotent (sqlx's version table prevents re-apply)
- [ ] All CHECK constraints listed above are present (verify by querying `sqlite_master`)
- [ ] Both partial unique indexes exist and have the correct WHERE predicate
- [ ] `source_successful_run_number_at_fetch` has no default and no update trigger (append-only design)

**Testing Expectations:**
- Integration test: apply migration to `:memory:`, inspect `sqlite_master` for all tables and indexes

---

### P0-T3: Seed data — categories and canonical items

**Type:** chore

**Description:**

Create `migrations/002_seed.sql` with the 11 categories and 226 canonical items ported from Shoppy.

**Categories** (use identical UUIDs from Shoppy — `c1000000-0000-0000-0000-000000000001` through `...000000000011`; values from `shoppy/supabase/migrations/002_seed_categories.sql`):

```sql
INSERT OR IGNORE INTO categories (id, name_en, name_es, icon, default_order) VALUES
  ('c1000000-0000-0000-0000-000000000001', 'Produce',     'Frutas y Verduras',  'carrot',      1),
  ('c1000000-0000-0000-0000-000000000002', 'Meat & Fish', 'Carne y Pescado',    'beef',        2),
  ('c1000000-0000-0000-0000-000000000003', 'Dairy',       'Lácteos',            'milk',        3),
  ('c1000000-0000-0000-0000-000000000004', 'Bakery',      'Panadería',          'bread-slice', 4),
  ('c1000000-0000-0000-0000-000000000005', 'Pantry',      'Despensa',           'package',     5),
  ('c1000000-0000-0000-0000-000000000006', 'Frozen',      'Congelados',         'snowflake',   6),
  ('c1000000-0000-0000-0000-000000000007', 'Snacks',      'Snacks',             'cookie',      7),
  ('c1000000-0000-0000-0000-000000000008', 'Drinks',      'Bebidas',            'cup-soda',    8),
  ('c1000000-0000-0000-0000-000000000009', 'Baby',        'Bebé',               'baby',        9),
  ('c1000000-0000-0000-0000-000000000010', 'Household',   'Hogar y Limpieza',   'spray-can',  10),
  ('c1000000-0000-0000-0000-000000000011', 'Toiletries',  'Higiene Personal',   'bath',       11);
```

**Canonical items and aliases:** Write a build script or `build.rs` that reads the Shoppy `dictionary.ts` (or a JSON export of it) and generates the SQL INSERT statements. Alternatively, hand-write them. For each `DictionaryEntry`:

- One row in `canonical_items`: `id` = deterministic `hex(SHA-256(household_id + ":" + slug))` where `slug` comes from the `slug` field in `fixtures/dictionary.json` (required — seed generation fails if missing). **Not** a random UUID v4. Using an explicit slug (rather than `normalize(name_en)`) means IDs survive display-name renames and are not sensitive to normalisation differences. `slug` is the permanent immutable identifier for a dictionary entry; it must never change even if `name_en` is corrected. `household_id` = a fixed dev household UUID (`00000000-0000-0000-0000-000000000001`), `name_en` = `name_en`/`termEn`, `name_es` = `name_es`/`termEs`, `category_id` = corresponding category UUID, `default_constraint_policy` = the entry's `default_constraint_policy` from `fixtures/dictionary.json` (default to `'any'` only when omitted). The DB seed and generated fixtures must use the same dictionary-derived policy so fixture and live mode cannot drift.
- One or more rows in `item_aliases`: one for `termEn` (language `en`, source `manual`), one for `termEs` (language `es`, source `manual`), and one per entry in `variants` (language inferred — English word → `en`, Spanish → `es`; default to `en` if uncertain)

Note the Shoppy normaliser removes punctuation completely (`"coca-cola" → "cocacola"`). The design doc specifies the Rust normaliser must differ: punctuation becomes a space (`"coca-cola" → "coca cola"`, exception for known brand compaction rules). Aliases stored in the seed must use the design doc's normalisation rules, not Shoppy's. Pre-normalise all alias strings before inserting (strip accents, lowercase, punctuation → space, whitespace collapse).

The fixed dev household UUID is used in Phase 0 only. Phase 1 creates real household records.

**Files / Areas Touched:**
- `migrations/002_seed.sql` (create)
- `fixtures/dictionary.json` (create: JSON export of all 226 entries; **must include a `slug` field** on every entry — slug is the stable machine identifier used to derive `canonical_items.id`. `default_constraint_policy` may be omitted and then defaults to `"any"`, but must be explicit for items with brand/other policies. Format: `{ "slug": "whole_milk", "name_en": "Whole Milk", "name_es": "Leche Entera", "category": "dairy", "default_constraint_policy": "any", "variants": [...] }`. The seed generator must fail with a clear error if any entry is missing a slug or if any two entries share the same slug. `fixtures/canonical_items.json` is **generated** from `dictionary.json` — it is not hand-maintained. Run `cargo run -p buywise-cli -- generate-fixtures` (or a build script) to regenerate it whenever `dictionary.json` changes. This is the single source of truth for IDs and policies.)

**Acceptance Criteria:**
- [ ] `SELECT COUNT(*) FROM categories` returns 11
- [ ] `SELECT COUNT(*) FROM canonical_items` returns 226 (approximately; exact count depends on dictionary)
- [ ] `SELECT COUNT(*) FROM item_aliases` returns ≥ 452 (two per item minimum)
- [ ] Querying `item_aliases WHERE alias = 'leche'` returns the row for `canonical_items.name_en = 'Milk'`
- [ ] All aliases are normalised (lowercase, accent-stripped, punctuation-to-space, whitespace-collapsed)
- [ ] No duplicate `(household_id, lower(alias), language)` — the unique index enforces this
- [ ] All `canonical_items.id` values are unique — assert `SELECT COUNT(DISTINCT id) = COUNT(*)` to catch any slug collisions in the seed data

**Testing Expectations:**
- Integration test: apply migrations 001 + 002 to `:memory:`, assert the row counts above
- Stability test: run the seed generation twice on `:memory:` and assert all IDs are identical between runs — catches any accidental randomness in the ID derivation

---

### P0-T4: Core domain types

**Type:** feature

**Description:**

Implement the domain structs and enums in `crates/core/src/domain.rs`. These are plain data types used throughout the workspace. Use `serde::{Serialize, Deserialize}` on all types, `sqlx::FromRow` on any type fetched from the database.

Types to define:

```rust
// Price amounts — never use f64 for money
pub type Cents = i64;       // price in euro cents (price × 100)
pub type Micros = i64;      // unit price in micros (price × 1_000_000)

pub enum Retailer { Tesco, Dunnes, SuperValu, Aldi, Lidl }
pub enum Source { BasketWatch, LidlDirect, Manual }
// Both enums must implement as_db_str() returning the exact DB CHECK constraint value:
//   Retailer::Tesco => "tesco", Retailer::Dunnes => "dunnes", Retailer::SuperValu => "supervalu", etc.
//   Source::BasketWatch => "basketwatch", Source::LidlDirect => "lidl_direct", Source::Manual => "manual"
// This is the canonical serialisation used by stable_product_id() and all DB writes.
pub enum ResolutionStatus { Unresolved, Resolved, Ambiguous }
pub enum ItemStatus { Active, Bought, Skipped, Deleted }
pub enum MatchStatus { Accepted, Rejected, Suggested, Pending }
pub enum SourceConfidence { High, Medium, Low }
pub enum FreshnessConfidence { High, Low, Unknown }
pub enum ConstraintPolicy { Any, Brand, AllowedSet, OwnLabel, Exact, Ask }
pub enum SubstitutionPolicy { Any, Ask, Strict }
pub enum SizeBasis { Grams, Millilitres, Each }
pub enum UnitPriceBasis { PerKg, PerL, PerEach, PerSheet, PerNappy, PerWash, PerPod, PerRoll }

pub struct RequestedSize {
    pub value: f64,       // normalised SI value: grams for weight, millilitres for volume, count for Each
    pub basis: SizeBasis, // Grams | Millilitres | Each — input units ("L", "kg", "cl") are converted
                          // during parsing, e.g. "1L" → { value: 1000.0, basis: Millilitres }
}

pub struct ParsedInput {
    pub text: String,                          // normalised text with count/size stripped
    pub count: u32,                            // how many items to buy (default 1)
    pub requested_size: Option<RequestedSize>, // size constraint for product matching, not multiplied by price
    pub original: String,
}
// Note: ParsedInput and RequestedSize live in domain.rs and are used by normalise.rs.

pub struct Category { pub id: String, pub name_en: String, pub name_es: String, pub icon: String, pub default_order: i32 }

pub struct CanonicalItem {
    pub id: String,
    pub household_id: String,
    pub name_en: String,
    pub name_es: String,
    pub category_id: String,
    pub default_constraint_policy: ConstraintPolicy,
    pub created_at: String,
}

pub struct ItemAlias {
    pub id: String,
    pub household_id: String,
    pub canonical_item_id: String,
    pub alias: String,
    pub language: String,
    pub source: String,  // "manual" | "accepted_match" | "llm"
    pub created_at: String,
}

pub struct ItemConstraints {
    pub id: String,
    pub canonical_item_id: String,
    pub brand: Option<String>,
    pub allowed_brands: Option<Vec<String>>,
    pub excluded_brands: Option<Vec<String>>,
    pub preferred_size_value: Option<f64>,
    pub preferred_size_basis: Option<SizeBasis>,
    pub min_size_value: Option<f64>,
    pub min_size_basis: Option<SizeBasis>,
    pub max_size_value: Option<f64>,
    pub max_size_basis: Option<SizeBasis>,
    pub pack_count: Option<i32>,
    pub form_factor: Option<String>,
    pub dietary_flags: Option<Vec<String>>,
    pub substitution_policy: SubstitutionPolicy,
    pub updated_at: String,
}

pub struct RetailerProduct {
    pub id: String,
    pub source: Source,
    pub retailer: Retailer,
    pub external_id: Option<String>,
    pub url: Option<String>,
    pub name: String,
    pub brand: Option<String>,
    pub category: Option<String>,
    pub pack_size_text: Option<String>,
    pub size_value: Option<f64>,
    pub size_unit: Option<String>,
    pub pack_count: Option<i32>,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

pub struct ProductOffer {
    pub id: String,
    pub retailer_product_id: String,
    pub price_cents: Cents,
    pub unit_price_eur_micros: Option<Micros>,
    pub unit_price_basis: Option<UnitPriceBasis>,
    pub previous_price_cents: Option<Cents>,
    pub loyalty_price_cents: Option<Cents>,
    pub loyalty_scheme: Option<String>,
    pub offer_text: Option<String>,
    pub offer_valid_until: Option<String>,
    pub source_snapshot_date: Option<String>,
    pub fetched_at: String,
    pub cache_expires_at: Option<String>,
    pub last_successful_source_run_id: Option<String>,
    pub source_successful_run_number_at_fetch: Option<i64>,
    pub source_confidence_at_ingest: SourceConfidence,
}

pub struct SourceRun {
    pub id: String,
    pub source: Source,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub requests_made: i32,
    pub items_fetched: i32,
    pub items_normalised: i32,
    pub status: String,
    pub error_message: Option<String>,
    pub successful_run_number: Option<i64>,
}

pub struct ProductMatch {
    pub id: String,
    pub household_id: String,
    pub canonical_item_id: String,
    pub retailer_product_id: String,
    pub status: MatchStatus,
    pub matched_by: String,
    pub created_at: String,
    pub updated_at: String,
}
```

SQLx's `FromRow` derive requires column names to match struct field names. For enums, implement `sqlx::Type` manually or use newtype wrappers. Alternatively, store enums as `TEXT` in SQLite and use `TryFrom<String>` — this is simpler for SQLite.

**Files / Areas Touched:**
- `crates/core/src/domain.rs`
- `crates/core/src/lib.rs` (ensure `pub use domain::*` or explicit re-exports)

**Acceptance Criteria:**
- [ ] All types compile without warnings
- [ ] `serde_json::to_string(&offer)` and `from_str` round-trip for `ProductOffer`
- [ ] `Cents` and `Micros` type aliases prevent accidental float arithmetic (reviewed manually)
- [ ] `sqlx::query_as::<_, RetailerProduct>(...)` compiles against a pool (in a #[tokio::test])

**Testing Expectations:**
- Unit tests: serde round-trip for each major struct
- Compile-time test: a test that opens `:memory:`, applies migrations, and reads zero rows using `query_as`

---

### P0-T5: Text normaliser

**Type:** feature

**Description:**

Implement `crates/core/src/normalise.rs`. Port Shoppy's `normalize.ts` and `parseQuantityAndUnit` to Rust, with the corrections specified in the design doc.

**Key difference from Shoppy:** Shoppy strips all punctuation. The design doc requires punctuation → space (`"coca-cola" → "coca cola"`). Exception: known brand compaction rules (e.g. `"7-up" → "7up"` — define a small static lookup table for these; keep it short and explicit).

**Functions to implement:**

```rust
/// Normalize text for matching:
/// 1. Lowercase
/// 2. NFD decomposition + strip combining marks (unicode-normalization crate)
/// 3. Punctuation → single space (not deletion). Exception: brand compaction table.
/// 4. Collapse multiple whitespace to single space
/// 5. Trim
pub fn normalize(text: &str) -> String

/// Capitalise first letter, leave rest unchanged.
pub fn to_display_name(text: &str) -> String

/// Parse count and size from free text.
/// Handles "2x", "x2", "dos leches" (count), "500g" (size), "leche 2L" (size), "dos litros leche" (size).
/// Word numbers: English zero–twelve + Spanish cero, uno/una, dos, tres, cuatro, cinco, seis, siete, ocho, nueve, diez, once, doce
pub fn parse_count_and_size(input: &str) -> ParsedInput
```

`parse_count_and_size` returns `ParsedInput` (defined in `domain.rs` — see P0-T4). `count` feeds `BasketItem.count` (multiplied by price). `requested_size` is a size constraint used during product matching — it is **not** multiplied by price. "500g rice" means "I want one 500g pack of rice", not "500 packs".

Use the `unicode-normalization` crate for NFD. Use `char::is_alphabetic` and `char::is_numeric` to identify non-punctuation characters.

**Files / Areas Touched:**
- `crates/core/src/normalise.rs`
- `crates/core/Cargo.toml` (add `unicode-normalization`)

**Acceptance Criteria:**
- [ ] `normalize("LECHE")` → `"leche"`
- [ ] `normalize("Jamón")` → `"jamon"`
- [ ] `normalize("coca-cola")` → `"coca cola"` (space, not deletion — this is different from Shoppy)
- [ ] `normalize("  multiple   spaces  ")` → `"multiple spaces"`
- [ ] `parse_count_and_size("dos litros leche")` → `ParsedInput { text: "leche", count: 1, requested_size: Some(RequestedSize { value: 2000.0, basis: Millilitres }), ... }` (normalised to ml; one 2L bottle)
- [ ] `parse_count_and_size("500g rice")` → `ParsedInput { text: "rice", count: 1, requested_size: Some(RequestedSize { value: 500.0, basis: Grams }), ... }`
- [ ] `parse_count_and_size("2x milk")` → `ParsedInput { text: "milk", count: 2, requested_size: None, ... }`
- [ ] `parse_count_and_size("milk")` → `ParsedInput { text: "milk", count: 1, requested_size: None, ... }`

**Testing Expectations:**
- Unit tests covering: all word numbers (EN + ES), Spanish long-form units, punctuation-to-space rule, accent stripping, brand compaction exceptions
- Port the test cases from `shoppy/src/lib/matching/normalize.test.ts` to Rust (adjusting for the punctuation behaviour difference)

---

### P0-T6: Bilingual matching — local/fixture mode

**Type:** feature

**Description:**

Implement `crates/core/src/matching.rs`. This is the resolution logic used by Phase 0 (fixture-driven) and later extended in Phase 3 (full cascade with LLM). Phase 0 only needs steps 1 and 2 of the cascade (alias lookup and token/fuzzy match). Steps 3 (LLM) and 4 (user clarification) are stubbed.

**Resolution cascade (see design §8):**

```rust
pub enum ResolutionOutcome {
    /// Single unambiguous match — resolved canonical_item_id
    SingleMatch { canonical_item_id: String, kind: MatchKind },
    /// Multiple plausible candidates — requires user selection
    MultipleCandidates { candidates: Vec<String> },  // canonical_item_ids
    /// No match found via alias or fuzzy — requires LLM or user
    NoMatch,
}

/// Step 1: exact alias lookup (normalised)
pub fn alias_lookup(
    alias: &str,
    household_id: &str,
    db: &SqlitePool,
) -> impl Future<Output = Result<Option<String>>>  // canonical_item_id or None

/// Step 2: token/fuzzy match against name_en, name_es, and all aliases
/// Returns SingleMatch, MultipleCandidates, or NoMatch
/// Matching and ranking rules are owned by design §8.
pub fn fuzzy_match(
    text: &str,
    household_id: &str,
    db: &SqlitePool,
) -> impl Future<Output = Result<ResolutionOutcome>>

/// Full cascade (Phase 0 implementation — LLM and user steps stubbed)
/// Returns ResolutionOutcome
pub async fn resolve(
    raw_text: &str,
    household_id: &str,
    db: &SqlitePool,
) -> Result<ResolutionOutcome>
```

Use the `strsim` crate for Levenshtein distance (provides `levenshtein` function). Matching thresholds and ranking are owned by design §8.

Phase 0 `resolve` calls step 1 then step 2. If step 2 returns `NoMatch` or `MultipleCandidates`, return those outcomes directly without calling LLM or user clarification (those are added in Phase 3).

**Files / Areas Touched:**
- `crates/core/src/matching.rs`
- `crates/core/Cargo.toml` (add `strsim`)

**Acceptance Criteria:**
- [ ] `resolve("leche", household_id, &pool)` returns `SingleMatch { canonical_item_id }` where the item maps to Milk
- [ ] `resolve("milk", ...)` returns `SingleMatch` for the same item (via alias)
- [ ] `resolve("jamon", ...)` returns `SingleMatch` for ham/jamón (alias lookup after normalisation strips accent)
- [ ] `resolve("cream", ...)` returns `MultipleCandidates` (sour cream, single cream, double cream etc. all present)
- [ ] `resolve("xyzqrst_unknown", ...)` returns `NoMatch`
- [ ] Alias lookup is always tried before fuzzy (order enforced by cascade)

**Testing Expectations:**
- Unit tests using in-memory SQLite with migrations 001 + 002 applied
- Port the Shoppy `DuplicateDetector.test.ts` and `dictionary.test.ts` test cases to Rust

---

### P0-T7: Unit-price normaliser

**Type:** feature

**Description:**

Extend `crates/core/src/normalise.rs` with size and unit-price normalisation. The optimiser compares unit prices, not shelf prices. All sizes must be converted to canonical base units before comparison (see design §7 item_constraints and §9).

**Base unit rules:**
- Weight → grams (`"2kg" → 2000g`, `"500g" → 500g`, `"1lb" → 454g`)
- Volume → millilitres (`"2L" → 2000ml`, `"500ml" → 500ml`, `"1pt" → 568ml`)
- Discrete count → each (`"12 nappies" → 12`, `"40 sheets" → 40`)

**Functions to add:**

```rust
pub enum NormalisedSize {
    Weight { grams: f64 },
    Volume { ml: f64 },
    Count { each: u32 },
}

/// Parse retailer pack-size text into a NormalisedSize.
/// Input: "2L", "500g", "12 pack", "40 sheets", "1 kg"
/// Returns None if unparseable.
pub fn parse_pack_size(text: &str) -> Option<NormalisedSize>

/// Compute unit price in micros (€ × 1_000_000) given shelf price in cents and pack size.
/// Returns None if pack size is zero or unparseable.
pub fn compute_unit_price_micros(
    price_cents: i64,
    size: &NormalisedSize,
    basis: UnitPriceBasis,
) -> Option<i64>

/// Validate and if necessary recompute the unit price from a BasketWatch offer.
/// BasketWatch provides unit prices; this function cross-checks them.
/// If the provided value is within 2% of the computed value, use the provided value.
/// If it differs by more than 2%, log a warning and use the computed value.
/// Returns the validated unit_price_eur_micros and unit_price_basis.
pub fn validate_unit_price(
    price_cents: i64,
    pack_size_text: Option<&str>,
    provided_micros: Option<i64>,
    provided_basis: Option<UnitPriceBasis>,
) -> (Option<i64>, Option<UnitPriceBasis>)
```

**Unit category mapping** (see design §9):

| Category keyword | UnitPriceBasis |
|---|---|
| milk, juice, liquid | PerL |
| rice, pasta, flour, dry | PerKg |
| napp* | PerNappy |
| laundry, wash, detergent | PerWash |
| pod, capsule | PerPod |
| toilet roll, kitchen roll | PerSheet (fallback PerRoll) |
| (default) | PerEach |

**Files / Areas Touched:**
- `crates/core/src/normalise.rs` (extend)

**Acceptance Criteria:**
- [ ] `parse_pack_size("2L")` → `Volume { ml: 2000.0 }`
- [ ] `parse_pack_size("500g")` → `Weight { grams: 500.0 }`
- [ ] `parse_pack_size("12 pack")` → `Count { each: 12 }`
- [ ] `compute_unit_price_micros(price_cents=299, size=Volume{ml:2000}, basis=PerL)` → `1_495_000` (€2.99 / 2L = €1.495/L × 1_000_000)
- [ ] `validate_unit_price` returns computed value with warning when provided and computed diverge by > 2%

**Testing Expectations:**
- Unit tests for each pack-size format
- Unit tests for `compute_unit_price_micros` with known price/size pairs
- Test the 2% validation tolerance edge cases

---

> **Shared ingest rule — deterministic product IDs (applies to all non-manual sources):**
> `retailer_products.id` must be derived as `hex(SHA-256(source + ":" + retailer + ":" + external_id))` rather than a random UUID v4. This guarantees the same `external_id` always maps to the same primary key, regardless of which database or which ingest run first created it. Without stable IDs, `fixtures/product_matches.csv` (which hardcodes `retailer_product_id` values) would break every time a fresh DB is populated. Expose this as a shared helper in `crates/ingest/src/product_id.rs` used by both BasketWatch and Lidl adapters:
> ```rust
> pub fn stable_product_id(source: Source, retailer: Retailer, external_id: &str) -> String {
>     // Accept typed enums, not raw strings, so callers cannot pass mismatched values.
>     // Source and Retailer must implement as_db_str() returning the canonical DB CHECK value.
>     let key = format!(
>         "{}:{}:{}",
>         source.as_db_str(),   // e.g. "basketwatch", "lidl_direct"
>         retailer.as_db_str(), // e.g. "tesco", "lidl"
>         external_id.trim()    // trimmed but not lowercased — retailers may use case-sensitive IDs
>     );
>     hex::encode(Sha256::digest(key.as_bytes()))
> }
> ```
> `Source` and `Retailer` enums (defined in `domain.rs`) must implement `as_db_str() -> &'static str` returning the exact value used in the migration CHECK constraints. This centralises serialization so a renamed enum variant cannot silently change the hash. Use the `sha2` and `hex` crates (already in the dependency list).

---

### P0-T8: BasketWatch ingest adapter

**Type:** feature

**Description:**

Implement `crates/ingest/src/basketwatch.rs`. This adapter fetches price data from the BasketWatch RapidAPI and normalises it into `retailer_products` + `product_offers` rows.

**Critical flow** (see design §7 `source_runs` comment and §12):

1. Insert a `source_runs` row with `status = 'running'` (short transaction, immediately committed — do not hold this transaction open)
2. Fetch all data from BasketWatch API (network I/O, fully outside any transaction)
3. Normalise fetched data into in-memory `Vec<RetailerProduct>` + `Vec<ProductOffer>` — parse and discard raw JSON immediately
4. On success: open `BEGIN IMMEDIATE` transaction:
   - `SELECT MAX(successful_run_number) FROM source_runs WHERE source = 'basketwatch'`
   - `UPDATE source_runs SET status='success', successful_run_number=MAX+1, finished_at=now(), items_fetched=N, items_normalised=M WHERE id=<run_id>`
   - For each product: use `INSERT INTO retailer_products (...) ON CONFLICT(source, retailer, external_id) WHERE source != 'manual' AND external_id IS NOT NULL DO UPDATE SET last_seen_at = excluded.last_seen_at`. **Never use `INSERT OR REPLACE`** — SQLite REPLACE is a DELETE + INSERT which breaks stable primary keys and invalidates any `product_offers` rows referencing the old id via FK.
   - For each offer: `INSERT INTO product_offers` with `last_successful_source_run_id` = this run's id and `source_successful_run_number_at_fetch` = MAX+1
   - COMMIT
5. On network or parse failure: short transaction: `UPDATE source_runs SET status='failed', error_message=..., finished_at=now() WHERE id=<run_id>`

**API guardrails:** Before each request, query `api_usage_events` for the current month. Enforce the three thresholds from §12 (warn at 3500, skip non-essential at 4500, hard stop at 4900). Insert a row into `api_usage_events` after each request batch.

**Network control:** Two separate env vars with distinct semantics:
- `BUYWISE_NO_NETWORK=1` — adapter skips live API calls and reads from `fixtures/offers.json` instead, but **still writes** `source_runs` and `product_offers` rows to the DB. Used by `--dry-run`. Enables full end-to-end pipeline testing without a real API key.
- `BUYWISE_LOCAL_ONLY=1` — `optimise` and `validate-basket` create an in-memory SQLite database, apply the Phase 0 schema migration only (`001_phase0_schema.sql`), seed it from fixture files, then run the normal resolver/optimiser code paths. No persistent SQLite file is created. The ingest adapter is not invoked. Used for pure in-process smoke tests without a DB file or network.

Do not conflate these: `BUYWISE_NO_NETWORK` controls whether the network is hit; `BUYWISE_LOCAL_ONLY` controls whether a persistent SQLite file is used. Local-only mode still uses `:memory:` SQLite so it exercises the same SQL-backed resolver and optimiser paths as live mode.

**Rate limiting:** self-imposed 40 RPM cap. Use `tokio::time::sleep` between request batches to stay below this. The 60 RPM limit is from BasketWatch's Pro plan; 40 RPM provides a safety buffer.

**Ingestion code must reject any API product row with a missing `external_id`** — treat it as a parse error, not a silent insert (which would bypass the partial unique index).

**Retailers covered:** Tesco, Dunnes Stores, SuperValu, Aldi (BasketWatch covers all four). Lidl is separate (P0-T9).

**Files / Areas Touched:**
- `crates/ingest/src/lib.rs` (expose modules)
- `crates/ingest/src/basketwatch.rs` (create)
- `crates/ingest/src/product_id.rs` (create: `stable_product_id` helper — shared with Lidl)
- `crates/ingest/src/error.rs` (create: `IngestError` enum)
- `crates/core/src/db.rs` (add query helpers for source_runs, retailer_products, product_offers)

**Acceptance Criteria:**
- [ ] With `BUYWISE_NO_NETWORK=1`, the adapter reads from `fixtures/offers.json` instead of making API calls, and still writes `source_runs` and `product_offers` rows to the DB
- [ ] In live mode, a `source_runs` row with `status='running'` is committed before any fetch
- [ ] After a successful run, `successful_run_number` = previous max + 1
- [ ] After a failed run, `successful_run_number` IS NULL on the run row
- [ ] Products with `external_id IS NULL` from the API are logged as errors and skipped — not inserted
- [ ] The partial unique index deduplication works: running the adapter twice for the same product updates `last_seen_at` but does not create a duplicate row
- [ ] `source_successful_run_number_at_fetch` on inserted offer rows matches the `successful_run_number` of the run
- [ ] No write transaction is held open across any network I/O

**Testing Expectations:**
- Unit tests for the normalisation step using mock JSON responses (no live network)
- Integration test using `BUYWISE_NO_NETWORK=1` that verifies `source_runs` and `product_offers` are written correctly (the adapter is invoked with a real DB, just no network)
- Integration test for deduplication: ingest fixture data twice with `BUYWISE_NO_NETWORK=1`, assert `retailer_products` count unchanged
- **Brand fixture test:** `fixtures/offers.json` must include at least one product with a non-null `brand` (e.g. Fairy washing-up liquid). After dry-run ingest, assert `SELECT brand FROM retailer_products WHERE name LIKE '%Fairy%'` returns a non-null value. This proves brand data flows through ingest so that brand constraints in the optimiser have real data to filter against. If the BasketWatch API does not return brand as a top-level field, document the extraction path (e.g. parsed from product name) so it is not silently NULL.

---

### P0-T9: Lidl direct adapter

**Type:** feature

**Description:**

Implement `crates/ingest/src/lidl.rs`. The Lidl.ie adapter fetches product data directly from Lidl.ie (see design §12: "Lidl.ie product search", confidence medium).

The adapter follows the same `source_runs` flow as BasketWatch (P0-T8). Use `stable_product_id` from `crates/ingest/src/product_id.rs` for all `retailer_products.id` values (see shared ingest rule above P0-T8). Set `source_confidence_at_ingest = 'medium'` on all Lidl offer rows.

**Isolation:** Lidl adapter failure must not affect the BasketWatch pipeline. Run them independently with separate error handling. A Lidl parse error should produce a failed `source_runs` row for `source='lidl_direct'` and leave BasketWatch data untouched.

The Lidl feed embeds size in the product name — the pack-size parser from P0-T7 is used to extract it.

Note: §16 open question — Lidl.ie reliability is uncertain. Build the adapter but gate it behind `BUYWISE_NO_NETWORK` for no-network testing (same as BasketWatch). In all modes, wrap the entire Lidl ingest in a `tokio::spawn` so a failure is logged but does not block or fail the BasketWatch run.

**Files / Areas Touched:**
- `crates/ingest/src/lidl.rs` (create)
- `crates/ingest/src/lib.rs` (expose)

**Acceptance Criteria:**
- [ ] Lidl adapter failure does not propagate as an error to the caller — it is logged and swallowed
- [ ] `source_confidence_at_ingest` = `'medium'` on all Lidl offer rows
- [ ] Pack sizes are extracted from product names via `parse_pack_size`
- [ ] Lidl products have their own `source_runs` rows (separate from BasketWatch)

**Testing Expectations:**
- Unit tests using fixture HTML/JSON (no live Lidl.ie calls in CI)
- Test that parse errors produce a `status='failed'` source_run, not a panic

---

### P0-T10: Optimiser — single-store and two-store plans

**Type:** feature

**Description:**

Implement `crates/core/src/optimiser.rs`. This is the Phase 0 core: given a list of `canonical_item_id`s (and their constraints), find the cheapest single-store and two-store combinations. The theoretical minimum (plan 3) and recommended practical plan (plan 4) are also built here.

**Input:**

```rust
pub type CanonicalItemId = String;  // defined here for use in Plan and OptimiserOutput

pub struct BasketItem {
    pub canonical_item_id: Option<CanonicalItemId>,  // None for unresolved entries (resolution_status != 'resolved')
    pub raw_text: String,                   // always present; used in unresolved_items output
    pub count: u32,
    pub constraint_policy: ConstraintPolicy, // copied from canonical_items.default_constraint_policy;
                                             // defaults to Any if canonical_item_id is None
    pub constraints: Option<ItemConstraints>, // merged constraints: load canonical item's row from
                                              // item_constraints (if any), then override
                                              // preferred_size_value/basis with ParsedInput.requested_size
                                              // if present. Never replace — merge, so brand + size both apply.
}

pub struct OptimiserInput {
    pub household_id: String,
    pub items: Vec<BasketItem>,
    pub convenience_threshold_2: Cents,   // default 1000 (€10.00)
    pub convenience_threshold_3: Cents,   // default 2500 (€25.00)
}
```

**Output:**

```rust
pub enum PlanType { BestSingleStore, BestTwoStore, TheoreticalMinimum, RecommendedPractical }

pub struct PlanItem {
    pub canonical_item_id: String,
    pub retailer_product_id: String,
    pub retailer: Retailer,
    pub selected_price_cents: Cents, // shelf/loyalty price for the chosen product (from product_offers, loyalty-adjusted);
                                     // this is the total price paid at the till for one of this product, not a per-kg/per-L unit price
    pub count: u32,                  // from BasketItem.count (how many to buy)
    pub line_total_cents: Cents,     // selected_price_cents * count; contributes to Plan.total_cents
    pub unit_price_eur_micros: Option<Micros>,
    pub unit_price_basis: Option<UnitPriceBasis>,
    pub source_confidence: SourceConfidence,
    pub freshness_confidence: FreshnessConfidence,
    pub offer_text: Option<String>,
    pub loyalty_scheme: Option<String>,
}

pub struct Plan {
    pub plan_type: PlanType,
    pub stores: Vec<Retailer>,
    pub items: Vec<PlanItem>,
    pub total_cents: Cents,
    pub missing_items: Vec<CanonicalItemId>,  // items with no offer at any store in this plan
    pub is_complete: bool,                    // missing_items.is_empty()
    pub saving_vs_best_single: Cents,         // 0 for BestSingleStore itself; computed over priced items only
    pub clears_convenience_threshold: bool,
}

pub struct UnresolvedItem {
    pub canonical_item_id: Option<String>,  // None for unresolved entries
    pub raw_text: String,
}

pub struct OptimiserOutput {
    pub best_single: Plan,
    pub best_two_store: Plan,
    pub theoretical_minimum: Plan,
    pub recommended: Plan,                      // points to one of the above by value
    pub unresolved_items: Vec<UnresolvedItem>,  // canonical_item_id IS NULL (language not resolved)
    pub no_valid_offer_items: Vec<CanonicalItemId>, // resolved canonical item but no accepted/suggested
                                                    // product_match exists, or all matches were rejected;
                                                    // excluded from all plan totals
    pub plain_language_explanation: String,  // generated by optimiser, not LLM
}
```

**Algorithm:**

**Pre-step (done by caller, before constructing `OptimiserInput`):** For each parsed basket entry, call `parse_count_and_size` on the raw text. The parser converts all input units to normalised SI values before returning:

| Input text | `requested_size.basis` | `requested_size.value` |
|---|---|---|
| `"500g"` | `Grams` | 500.0 |
| `"1kg"` | `Grams` | 1000.0 |
| `"250ml"` | `Millilitres` | 250.0 |
| `"25cl"` | `Millilitres` | 250.0 |
| `"2L"` / `"dos litros"` | `Millilitres` | 2000.0 |
| `"6 pack"` / `"x6"` | `Each` | 6.0 |

For each resolved basket entry (where `canonical_item_id` is known), the caller must also:
1. Load `canonical_items.default_constraint_policy` → set `BasketItem.constraint_policy`.
2. Load the `item_constraints` row for this `canonical_item_id` (if any) → this is the base constraints object.
3. If `ParsedInput.requested_size` is `Some(size)`, **merge** it into the base constraints: set/override `preferred_size_value = size.value` and `preferred_size_basis = size.basis`. This merged object becomes `BasketItem.constraints`.

The merge preserves brand and other fields from `item_constraints` — "Fairy 500ml" keeps `brand = 'Fairy'` and adds the 500ml size. `constraint_policy` travels alongside `constraints` so the optimiser's 1c filter can enforce the `brand` policy error path (policy=brand but brand field NULL → config error → `no_valid_offer_items`).

1. For each `BasketItem`:
   - **1a.** If `canonical_item_id IS NULL` → add to `unresolved_items` (language/item not resolved yet); skip.
   - **1b.** Otherwise load `product_matches WHERE status IN ('accepted', 'suggested') AND household_id = ? AND canonical_item_id = ?`. Load all non-rejected candidates so the optimiser can compare every valid product across all stores. The `accepted`/`suggested` distinction is a **confidence signal, not a priority gate**: for pricing and plan selection, the primary criterion is always cheapest price. `accepted` status is used only as a tie-breaker when two products have the same price — prefer the accepted match. If no candidates exist at all → add to `no_valid_offer_items`; skip. **Do not add to `unresolved_items` — the canonical item is known.**
   - **1c. Filter by constraints (Phase 0 scope):** If `BasketItem.constraints` is `Some(c)`, join each candidate match to `retailer_products` and apply the following filters in order. If no candidates survive, treat the item as having no valid offer (→ `no_valid_offer_items`); **never fall back to unfiltered candidates** — a 500g pack is not a substitute for a 1kg pack, and a generic brand is not a substitute for a named brand.
     - **`default_constraint_policy` in Phase 0:** the only policy values that affect Phase 0 filtering are `any` (no additional filtering) and `brand` (enforce the brand constraint from `item_constraints.brand` for this canonical item). The brand value comes exclusively from `item_constraints.brand`; `canonical_items` has no primary brand field. If the policy is `brand` but the `item_constraints` row is missing or its `brand` field is NULL, this is a **configuration error** — do not fall back to `any`. Instead, treat the item as if all candidates were filtered out (→ `no_valid_offer_items`) and log a structured warning: `"canonical_item {id} has policy=brand but no brand constraint set"`. Silently relaxing a brand constraint produces wrong recommendations. All other policy values (`allowed_set`, `own_label`, `exact`, `ask`) are stored but not enforced in Phase 0; they are part of Backlog-T1. If the seed dictionary leaves all policies as `any`, this branch is only exercised by targeted fixtures; if any seed item uses `brand`, it must have a matching `item_constraints.brand` row.
     - **Size** (Phase 0): if `c.preferred_size_value` is set, accept only products whose `size_value` is within ±20% of the preferred value and whose `size_unit` normalises to the same `SizeBasis`. "500g rice" must not match a 1kg bag.
     - **Brand** (Phase 0): if `c.brand` is `Some(b)`, accept only products where `normalize(retailer_products.brand) == normalize(b)`. "Fairy" must not match Tesco own-label washing-up liquid.
     - **Allowed/excluded brands, pack count, form factor, dietary flags, substitution policy** — **out of Phase 0 scope**. These fields are stored in `ItemConstraints` and in the schema but are not evaluated by the optimiser yet. They must be enforced before the app is useful for nappies size selection, cans vs bottles, zero sugar, or dietary requirements (dairy-free, gluten-free). This is a **named gap**: see the post-Phase-5 backlog task "Full constraint enforcement" to be added when Phase 5 is complete. Do not silently discard these fields at ingest.
2. For each accepted/suggested match (post-filter), load the latest offer per `retailer_product_id` (ORDER BY `fetched_at DESC LIMIT 1`). Apply loyalty prices: use `loyalty_price_cents` if non-null and the scheme is `'clubcard'` or `'real_rewards'` (household is assumed to have both cards). The resolved shelf price becomes `selected_price_cents`. Multiply by `BasketItem.count` to get `line_total_cents`. `Plan.total_cents` = sum of all `PlanItem.line_total_cents`.
**Fulfilment rule (applies to all plan types):** rank candidate plans by `(missing_count ASC, total_cost ASC)`. A plan missing 3 items can never beat a plan missing 0 items, even if its priced subtotal is lower. Each `Plan` carries `missing_items: Vec<CanonicalItemId>` and `is_complete: bool` so the API and UI can explain why a cheaper subtotal lost.

3. **Single-store plan:** For each of the 5 retailers, compute the total cost using that retailer's best matching product per item; track items with no offer at this retailer in `missing_items`. Apply the fulfilment rule; pick the candidate with `(fewest missing, lowest cost)`.
4. **Two-store plan:** For each pair of the 5 retailers, assign each item to the cheaper store (falling back to the other if one store has no offer). Track items missing at both stores in `missing_items`. Apply the fulfilment rule to select the best pair.
5. **Theoretical minimum:** Each item goes to its cheapest retailer across all 5 stores. An item missing at all retailers goes into `missing_items`. Apply the fulfilment rule (here `missing_items` reflects items with no offer anywhere — these are also in `no_valid_offer_items`).
6. **Recommended plan:** Completeness is always primary; convenience thresholds only apply among plans with equal `missing_count`:
   - Compare `best_single.missing_items.len()` vs `best_two_store.missing_items.len()`.
   - Fewer missing → that plan wins outright, regardless of cost or convenience threshold.
   - Equal missing count (including both complete) → apply the convenience threshold: `best_single.total_cents - best_two_store.total_cents > convenience_threshold_2` → recommend two-store; otherwise recommend best single.
   - For three stores: same completeness-first logic applied to `theoretical_minimum` (when it uses ≤ 3 stores) vs `best_two_store`; threshold is `convenience_threshold_3` incremental.
   - Never let a subtotal comparison override a missing-item difference in either direction.

**Freshness confidence computation** (see design §7 and §12):
```
missed_cycles = current_source_successful_run_number - offer.source_successful_run_number_at_fetch
```
- `missed_cycles = 0` → `High`
- `missed_cycles = 1` → `Low`
- `missed_cycles >= 2` → `Unknown`
- `current_source_run_number IS NULL` (no successful run yet — Phase 0 fixture data) → `Unknown`

This is computed per offer at query time — no additional JOIN needed beyond the denormalised field.

**Dunnes voucher modelling** (see design §10): If the Dunnes basket total is ≥ 4500 and < 5000 cents (€45–€50), flag it in the plan output (`offer_text` on the plan, not on individual items): "Dunnes: add €X.XX more to potentially trigger the €50 voucher." Do not subtract the voucher from the basket total.

**Files / Areas Touched:**
- `crates/core/src/optimiser.rs`
- `crates/core/src/db.rs` (add query helpers for product_matches + product_offers)

**Acceptance Criteria:**
- [ ] With fixture data: `best_single.stores` has exactly one retailer, `best_two_store.stores` has exactly two
- [ ] Unresolved items appear in `unresolved_items` and are excluded from `total_cents`
- [ ] Among complete plans only (all `is_complete = true`): `theoretical_minimum.total_cents` ≤ `best_two_store.total_cents` ≤ `best_single.total_cents`. If no complete plan exists, each plan's `missing_items` is non-empty and the invariant is not required to hold (a two-store plan missing 1 item may cost less than a single-store plan missing 2 items even though it visits more stores).
- [ ] Loyalty prices (Clubcard, Real Rewards) are used when present
- [ ] Freshness confidence is `Unknown` when `current_source_run_number IS NULL`
- [ ] Dunnes voucher flag appears when applicable
- [ ] `recommended.clears_convenience_threshold = false` when saving < €10
- [ ] **Brand constraint rejection:** with a fixture `item_constraints` row for "washing-up liquid" (`canonical_item_id = <Fairy item>`, `brand = 'Fairy'`) and fixture candidates including one Fairy product and one own-label product, only the Fairy product appears in the plan — the own-label candidate is excluded
- [ ] If brand constraint eliminates all candidates for an item, the item appears in `no_valid_offer_items`, not `unresolved_items`

**Testing Expectations:**
- Unit tests with in-memory SQLite and fixture data covering: single-store best, two-store split, loyalty price application, freshness confidence computation, convenience threshold boundary values, Dunnes voucher boundary (€44.99, €45.00, €49.99, €50.00)
- Add `fixtures/item_constraints.json` (one row per item that has a non-`any` constraint in Phase 0 tests, e.g. `{ "canonical_item_id": "<fairy-id>", "brand": "Fairy" }`); load this in optimiser unit tests alongside `fixtures/offers.json`

---

### P0-T11: CLI commands

**Type:** feature

**Description:**

Implement the five CLI commands in `crates/cli/src/main.rs` using `clap` derive macros.

**Commands:**

```
buywise-cli validate-basket <fixture-path>
buywise-cli optimise --list <fixture-path> [--db <sqlite-path>] [--threshold2 <cents>] [--threshold3 <cents>]
buywise-cli ingest-basketwatch --db <sqlite-path> [--dry-run]
buywise-cli seed-matches --from <csv-path> --db <sqlite-path> --household-id <uuid>
buywise-cli generate-fixtures
```

**`validate-basket <fixture-path>`:**
Load `fixtures/items.json` (an array of `{ raw_text, expected_canonical }` pairs). Unless `--db` is added later, this command uses the same fixture bootstrap as `optimise` fixture mode: create an in-memory SQLite database, apply the Phase 0 schema migration only (`001_phase0_schema.sql`), seed categories/canonical items/aliases from `fixtures/canonical_items.json`, then run `resolve()` from P0-T6. Do not apply `002_seed.sql` in fixture mode, because the generated fixture file supplies the seed data. Print a table: raw text | resolution outcome | expected | match/fail. Print summary: N resolved, M unresolved, P ambiguous, Q failures. This command validates the resolution cascade against known-good mappings.

**`optimise --list <fixture-path> [--db <sqlite-path>]`:**
Load a basket file of the form:
```json
[
  { "raw_text": "leche entera 2L", "count": 1 },
  { "raw_text": "pan de molde", "count": 1 },
  ...
]
```
Two modes determined by the presence of `--db`:
- **Fixture mode** (no `--db`): creates an **in-memory SQLite database**, applies the Phase 0 schema migration only (`001_phase0_schema.sql`), seeds it from `fixtures/canonical_items.json`, `fixtures/item_constraints.json`, `fixtures/offers.json`, and `fixtures/product_matches.csv`, then runs the full resolution and optimisation pipeline against it. Do not apply `002_seed.sql` here, because the generated fixture file supplies categories, canonical items, and aliases. No persistent DB file is created. This reuses the same resolver and optimiser code paths as live mode — there is no parallel no-DB code path. `canonical_items.json` supplies categories, canonical item rows, aliases, and policy data; `item_constraints.json` supplies constraint values; the in-memory DB supports the policy-driven config-error path and brand filtering in CI.
- **Live mode** (`--db buywise.db`): reads `retailer_products`, `product_offers`, and `product_matches` from the specified SQLite database (populated by `ingest-basketwatch` + `seed-matches`). This is the mode used for the Phase 0 gate.

Print for both modes:
- Four plans with their store assignments and totals
- Recommended plan with plain-language explanation
- Unresolved items and `no_valid_offer_items`
- Estimated monthly saving (multiply weekly saving × 4.33)

**`ingest-basketwatch --db <sqlite-path> [--dry-run]`:**
`--db` is required — all ingest writes go to this SQLite file. Without `--dry-run`, runs live (requires `BASKETWATCH_API_KEY` env var and makes real network calls). With `--dry-run`, sets `BUYWISE_NO_NETWORK=1` internally: reads from `fixtures/offers.json` but still writes `source_runs` and `product_offers` rows to the specified DB, so the pipeline from ingest → optimise can be verified end-to-end without a live API key.

**Fixture files to create:**
- `fixtures/items.json` — validation basket of ~30 items covering all 11 categories, mix of EN and ES text, some ambiguous terms
- `fixtures/weekly-shop.json` — representative weekly shop (~40 items) for savings estimation. **Phase 0 constraint scope:** only include items whose optimisation is valid under Phase 0 filtering (size and brand constraints only). Do not include items where correctness depends on form_factor, dietary flags, or allowed/excluded brands — or mark such rows with a `"constraint_gap": true` field. Practical guidance: branded items like Fairy (brand-filterable), staples with no constraint, and items with a distinct canonical slug per size/variant (e.g. `nappies_size_4` and `nappies_size_5` as separate canonical items, not a single "nappies" item with a size constraint) are safe in Phase 0 — size is baked into the canonical identity, not evaluated at runtime. Items whose variant selection requires runtime constraint evaluation (form_factor, dietary flags, cans vs bottles) are not safe in Phase 0.
- `fixtures/offers.json` — static snapshot of BasketWatch-format data for a subset of items, used when `BUYWISE_NO_NETWORK=1` (dry-run ingest) or `BUYWISE_LOCAL_ONLY=1` (in-memory SQLite fixture mode)
- `fixtures/product_matches.csv` — pre-defined `canonical_item → retailer_product` mappings for the validation basket; same CSV format as `product_matches_live.csv` so `seed-matches` accepts both with no format branching
- `fixtures/canonical_items.json` — **generated from `fixtures/dictionary.json`**, not hand-maintained. Contains everything needed to seed fixture-mode lookup tables without rereading the dictionary: `categories`, `canonical_items` rows (`id`, `household_id`, `slug`, `name_en`, `name_es`, `category_id`, `default_constraint_policy`, `created_at`) and `item_aliases` rows (`id`, `household_id`, `canonical_item_id`, `alias`, `language`, `source`, `created_at`). `slug` is fixture metadata used to audit deterministic IDs and is ignored when inserting into the DB, because `canonical_items` has no `slug` column. Regenerate with `cargo run -p buywise-cli -- generate-fixtures` whenever `dictionary.json` changes. Committed to the repo so CI does not need to run the generator; the generator is run locally and the output is committed.
- `fixtures/item_constraints.json` — array of `{ canonical_item_id, brand?, preferred_size_value?, preferred_size_basis? }` rows used in fixture mode; must include at least one brand-constrained item (e.g. Fairy) to exercise brand filtering in CI

**`seed-matches --from <csv-path> --db <sqlite-path> --household-id <uuid>`:**
CSV columns: `(canonical_item_id, retailer_product_id, status, matched_by)` — no `household_id` in the file; `--household-id` is required and supplies it for every row. For Phase 0, always pass `--household-id 00000000-0000-0000-0000-000000000001`. This avoids ambiguity and makes the CSV portable across environments. Because `retailer_products.id` is a deterministic SHA-256 hash of `(source, retailer, external_id)` (see P0-T8), the `retailer_product_id` values in the CSV are stable across fresh DB populations — the same product from BasketWatch always gets the same ID.

Upserts into `product_matches` using:
```sql
INSERT INTO product_matches (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, created_at, updated_at)
VALUES (?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
ON CONFLICT(household_id, canonical_item_id, retailer_product_id)
DO UPDATE SET status = excluded.status, matched_by = excluded.matched_by, updated_at = excluded.updated_at;
```
**Do not use `INSERT OR REPLACE`** — REPLACE deletes and reinserts, breaking `match_feedback.product_match_id` FK references that will exist in Phase 5.

`--db` is required: this command always writes to SQLite; there is no in-memory fixture mode.

The workflow: run `ingest-basketwatch --db buywise.db`, query `retailer_products` for recognisable products, map them to canonical items by hand in `fixtures/product_matches_live.csv`, then `seed-matches --from fixtures/product_matches_live.csv --db buywise.db --household-id 00000000-0000-0000-0000-000000000001`. This is intentionally low-tech for Phase 0 — Phase 5 adds the automated match-feedback loop.

**Item constraints in live mode:** the Phase 0 seed migration uses `default_constraint_policy` from `fixtures/dictionary.json`, but it does not automatically create `item_constraints` rows for every constrained item. If an entry uses `default_constraint_policy = 'brand'`, live validation must also seed the corresponding `item_constraints.brand` row, or the optimiser will correctly mark that item as a configuration error and place it in `no_valid_offer_items`. For Phase 0 there are two valid approaches: keep dictionary policies as `any` and include only constraint-valid products in `product_matches_live.csv`, or add a small `seed-constraints --from fixtures/item_constraints.json --db buywise.db` command before optimisation. Do not rely on product matches alone for any item whose dictionary policy is `brand`.

**`generate-fixtures`:**
Reads `fixtures/dictionary.json` and writes `fixtures/canonical_items.json`. Fails with a clear error if any entry is missing a `slug`, has a duplicate slug, references an unknown category, or has an invalid `default_constraint_policy` value. The generated file must contain enough data to insert rows into `categories`, `canonical_items`, and `item_aliases` in an in-memory SQLite database; fixture-only metadata such as `slug` must be skipped by the DB insert code. Run locally and commit the output; CI does not run the generator (it uses the committed file). This is the only command that touches `fixtures/canonical_items.json` — never edit it by hand.

**Files / Areas Touched:**
- `crates/cli/src/main.rs`
- `crates/cli/src/commands/` (create subdirectory: `validate.rs`, `optimise.rs`, `ingest.rs`, `seed_matches.rs`, `generate_fixtures.rs`)
- `fixtures/items.json` (create)
- `fixtures/weekly-shop.json` (create)
- `fixtures/offers.json` (create)
- `fixtures/product_matches.csv` (create — used in local/dry-run mode)
- `fixtures/canonical_items.json` (generated from `dictionary.json` — do not edit by hand; commit the output)
- `fixtures/item_constraints.json` (create — loaded by fixture mode alongside offers.json; must include at least one brand-constrained item)
- `fixtures/product_matches_live.csv` (create after first live ingest run — not committed with dummy data)

**Acceptance Criteria:**
- [ ] `buywise-cli validate-basket fixtures/items.json` runs without panic; prints resolution table
- [ ] `buywise-cli optimise --list fixtures/weekly-shop.json` prints four plans and a saving estimate
- [ ] `buywise-cli ingest-basketwatch --db test.db --dry-run` completes without network calls (sets `BUYWISE_NO_NETWORK=1`); writes a `source_runs` row and fixture offer rows to `test.db`; prints "dry-run mode"
- [ ] `buywise-cli seed-matches --from fixtures/product_matches.csv --db test.db --household-id 00000000-0000-0000-0000-000000000001` upserts rows and prints count; running it twice is idempotent. **Prerequisites:** fresh DB with migrations applied + `ingest-basketwatch --db test.db --dry-run` already run (to populate `retailer_products` so FK constraints on `retailer_product_id` are satisfied)
- [ ] After `ingest-basketwatch --db buywise.db` (live) + `seed-matches --from fixtures/product_matches_live.csv --db buywise.db --household-id 00000000-0000-0000-0000-000000000001`, `buywise-cli optimise --list fixtures/weekly-shop.json --db buywise.db` produces a non-zero savings estimate
- [ ] Estimated monthly saving is printed as a number; this is the Phase 0 gate value

**Phase 0 gate:** The printed monthly saving (from live data, after `seed-matches`) must be ≥ €50 to proceed. Document the result in `docs/phase0-results.md`.

**Two operating modes:**
- **Fixture smoke test** (`BUYWISE_LOCAL_ONLY=1`, no `--db`): `optimise` and `validate-basket` use an **in-memory SQLite database** seeded from the fixture files at startup — they apply the Phase 0 schema migration only (`001_phase0_schema.sql`) and insert rows from `canonical_items.json`, `item_constraints.json`, `offers.json`, and `product_matches.csv` into `:memory:` before running. This avoids re-implementing a parallel no-DB resolution path and avoids double-seeding from `002_seed.sql`. `weekly-shop.json` still contains only `raw_text` (resolution runs normally against the in-memory DB). No persistent SQLite file is created; the in-memory DB is discarded after the command exits.
- **Live validation**: `ingest-basketwatch --db buywise.db` → `seed-matches --from fixtures/product_matches_live.csv --db buywise.db --household-id 00000000-0000-0000-0000-000000000001` → `optimise --list fixtures/weekly-shop.json --db buywise.db`; produces the Phase 0 gate value; run manually before proceeding to Phase 1.

**Testing Expectations:**
- Integration test that runs `validate-basket` on `fixtures/items.json` and asserts ≥ 90% resolution rate
- Integration test that runs `optimise` with fixture data (all plans complete): assert `theoretical_minimum.total_cents ≤ best_two_store.total_cents ≤ best_single.total_cents` and all three `is_complete = true`. Fixture data must include product matches for every item in `fixtures/weekly-shop.json` at every retailer to guarantee completeness.

---

## Phase 1 — Backend API

**Gate:** All endpoints return correct responses for a two-user household. Auth flow complete. SSE stream delivers mutations to a second browser tab.

---

### P1-T1: Remaining SQLite migrations

**Type:** chore

**Description:**

Create `migrations/003_phase1_schema.sql` with the tables deferred from Phase 0:

```sql
CREATE TABLE households (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,  -- argon2id hash
    display_name  TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE TABLE household_members (
    household_id TEXT NOT NULL REFERENCES households(id),
    user_id      TEXT NOT NULL REFERENCES users(id),
    joined_at    TEXT NOT NULL,
    PRIMARY KEY (household_id, user_id)
);

CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,   -- opaque random token used as cookie value
    user_id    TEXT NOT NULL REFERENCES users(id),
    token_hash TEXT NOT NULL UNIQUE,   -- SHA-256 of the session token; never store raw token
    csrf_token TEXT NOT NULL,           -- readable cookie value; validated server-side
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_sessions_token_hash ON sessions(token_hash);
CREATE INDEX idx_sessions_expires_at ON sessions(expires_at);

CREATE TABLE shopping_lists (
    id               TEXT PRIMARY KEY,
    household_id     TEXT NOT NULL REFERENCES households(id),
    name             TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','archived')),
    current_revision INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    archived_at      TEXT
);

CREATE TABLE list_entries (
    id                          TEXT PRIMARY KEY,   -- client-generated UUID v4
    list_id                     TEXT NOT NULL REFERENCES shopping_lists(id),
    canonical_item_id           TEXT REFERENCES canonical_items(id),  -- NULLABLE until resolved
    resolution_status           TEXT NOT NULL DEFAULT 'unresolved'
        CHECK(resolution_status IN ('unresolved','resolved','ambiguous')),
    -- Invariants (all must be in CREATE TABLE; cannot be added via ALTER TABLE in SQLite):
    -- resolved → canonical_item_id IS NOT NULL
    CHECK(resolution_status != 'resolved' OR canonical_item_id IS NOT NULL),
    -- canonical_item_id NOT NULL → resolved
    CHECK(canonical_item_id IS NULL OR resolution_status = 'resolved'),
    -- unresolved → resolution_candidates_json IS NULL
    CHECK(resolution_status != 'unresolved' OR resolution_candidates_json IS NULL),
    -- ambiguous → resolution_candidates_json IS NOT NULL
    CHECK(resolution_status != 'ambiguous' OR resolution_candidates_json IS NOT NULL),
    -- ambiguous → non-empty array
    CHECK(resolution_status != 'ambiguous' OR json_array_length(resolution_candidates_json) > 0),
    -- resolved → no candidates
    CHECK(resolution_status != 'resolved' OR resolution_candidates_json IS NULL),
    resolution_candidates_json  TEXT,               -- JSON array of canonical_item_ids; ambiguous only
    raw_text                    TEXT NOT NULL,
    display_name                TEXT NOT NULL,
    added_by                    TEXT NOT NULL REFERENCES users(id),
    quantity                    INTEGER NOT NULL DEFAULT 1,
    status                      TEXT NOT NULL DEFAULT 'active'
        CHECK(status IN ('active','bought','skipped','deleted')),
    bought_by                   TEXT REFERENCES users(id),
    bought_at                   TEXT,
    effective_constraint_hash   TEXT NOT NULL,
        -- sentinel: 'unresolved:<entry_id>' when canonical_item_id IS NULL
        -- sentinel: 'default' when resolved with empty constraint set
        -- otherwise: SHA-256 hex of sorted-key JSON of resolved constraints
    last_updated_revision       INTEGER NOT NULL DEFAULT 0,
    updated_at                  TEXT NOT NULL
);
-- Dedup index: prevents two active rows for the same resolved item with the same constraints
CREATE UNIQUE INDEX uq_list_entries_active_dedup
    ON list_entries(list_id, canonical_item_id, effective_constraint_hash)
    WHERE status = 'active' AND canonical_item_id IS NOT NULL;

CREATE TABLE list_entry_events (
    id         TEXT PRIMARY KEY,
    entry_id   TEXT NOT NULL REFERENCES list_entries(id),
    user_id    TEXT NOT NULL REFERENCES users(id),
    event_type TEXT NOT NULL,
    payload    TEXT,    -- JSON
    created_at TEXT NOT NULL
);

CREATE TABLE mutations (
    id                TEXT PRIMARY KEY,   -- server-assigned UUID
    client_mutation_id TEXT NOT NULL UNIQUE,   -- client UUID; idempotency key
    device_id         TEXT NOT NULL,
    user_id           TEXT NOT NULL REFERENCES users(id),
    list_id           TEXT NOT NULL REFERENCES shopping_lists(id),
    entity_type       TEXT NOT NULL,
    entity_id         TEXT NOT NULL,
    operation_type    TEXT NOT NULL
        -- `set_quantity` added by migration 008 (issue #60); the inline CHECK here
        -- reflects the current canonical enum after all migrations.
        CHECK(operation_type IN ('add_item','mark_bought','mark_skipped','delete_item',
                                  'rename_item','resolve_alias','reject_match','set_constraint',
                                  'set_quantity')),
    payload           TEXT NOT NULL,    -- JSON
    base_revision     INTEGER NOT NULL,
    created_at_client TEXT NOT NULL,
    received_at_server TEXT NOT NULL,
    server_revision   INTEGER,          -- NULL until accepted
    status            TEXT NOT NULL DEFAULT 'accepted'
        CHECK(status IN ('accepted','rejected','merged')),
    response_json     TEXT              -- stored response body; required for idempotent replay on duplicate client_mutation_id
);
CREATE INDEX idx_mutations_list_server_revision
    ON mutations(list_id, server_revision);

CREATE TABLE store_category_orders (
    id           TEXT PRIMARY KEY,
    household_id TEXT NOT NULL REFERENCES households(id),
    store        TEXT NOT NULL CHECK(store IN ('tesco','dunnes','supervalu','aldi','lidl')),
    category_id  TEXT NOT NULL REFERENCES categories(id),
    display_order INTEGER NOT NULL,
    updated_at   TEXT NOT NULL,
    UNIQUE(household_id, store, category_id)
);

-- api_usage_events is defined in Phase 0 (001_phase0_schema.sql); do not redefine here.

CREATE TABLE recommendation_runs (
    id                       TEXT PRIMARY KEY,
    list_id                  TEXT NOT NULL REFERENCES shopping_lists(id),
    household_id             TEXT NOT NULL REFERENCES households(id),
    run_at                   TEXT NOT NULL,
    offer_snapshot_date      TEXT,
    plans_json               TEXT NOT NULL,    -- all four plans serialised as JSON
    recommended_plan         TEXT NOT NULL,
    convenience_threshold_2  INTEGER NOT NULL,  -- cents
    convenience_threshold_3  INTEGER NOT NULL
);

CREATE TABLE savings_events (
    id                             TEXT PRIMARY KEY,
    list_id                        TEXT NOT NULL REFERENCES shopping_lists(id),
    recommendation_run_id          TEXT REFERENCES recommendation_runs(id),
    baseline_cost_cents            INTEGER NOT NULL,
    recommended_cost_cents         INTEGER NOT NULL,
    theoretical_minimum_cost_cents INTEGER NOT NULL,
    chosen_basket_cost_cents       INTEGER,    -- manual entry after shop
    chosen_plan                    TEXT,
    chosen_stores                  TEXT,       -- JSON array
    gross_saving_cents             INTEGER NOT NULL,
    accepted_savings_cents         INTEGER,
    rejected_savings_cents         INTEGER,
    travel_cost_cents              INTEGER,
    subscription_cost_alloc_cents  INTEGER,
    net_saving_cents               INTEGER,
    created_at                     TEXT NOT NULL
);

CREATE TABLE match_feedback (
    id               TEXT PRIMARY KEY,
    product_match_id TEXT NOT NULL REFERENCES product_matches(id),
    user_id          TEXT NOT NULL REFERENCES users(id),
    action           TEXT NOT NULL CHECK(action IN ('accept','reject','prefer')),
    created_at       TEXT NOT NULL
);
```

Also create `migrations/004_seed_phase1.sql`:

```sql
-- IMPORTANT: use the same household UUID that Phase 0 seeded canonical_items with.
-- The Phase 0 migration uses 00000000-0000-0000-0000-000000000001 as household_id.
-- If Phase 1 seeds a different household UUID, /api/items returns nothing for real users.
INSERT OR IGNORE INTO households (id, name, created_at) VALUES
  ('00000000-0000-0000-0000-000000000001', 'Home', datetime('now'));

-- Seed the two household users (password hashes provided at deploy time via env vars or seed script)
-- Seed default store_category_orders for all 5 retailers × 11 categories
-- Default ordering per store mirrors Shoppy's store_profiles.category_order
```

The default `store_category_orders` rows: insert 55 rows (5 stores × 11 categories). Use a sensible aisle ordering for Irish supermarkets (e.g. Tesco: PRODUCE → BAKERY → DAIRY → MEAT_FISH → FROZEN → DRINKS → PANTRY → SNACKS → HOUSEHOLD → TOILETRIES → BABY). Exact ordering per store is an editorial call; document the choices in a comment in the migration.

**Files / Areas Touched:**
- `migrations/003_phase1_schema.sql` (create)
- `migrations/004_seed_phase1.sql` (create)

**Acceptance Criteria:**
- [ ] `sqlx migrate run` applies all four migrations to a fresh DB without error
- [ ] All CHECK constraints on `list_entries` are present — especially the six `resolution_status` invariants
- [ ] `uq_list_entries_active_dedup` partial unique index exists with the correct predicate
- [ ] `sessions` has both `token_hash` and `csrf_token` columns
- [ ] `store_category_orders` contains 55 rows after seeding (5 stores × 11 categories)
- [ ] `mutations.client_mutation_id` has a UNIQUE index

**Testing Expectations:**
- Integration test: apply all four migrations to `:memory:`, assert `store_category_orders` row count = 55
- Test the `list_entries` CHECK constraints by attempting to INSERT invalid states and asserting they fail

---

### P1-T2: Auth endpoints

**Type:** feature

**Description:**

Implement the auth routes in `crates/api/src/auth.rs`:

- `POST /api/auth/login` — validate password, create session, issue two cookies
- `POST /api/auth/logout` — delete session record, clear cookies
- `GET /api/auth/me` — return current user if session is valid

**Session and CSRF design** (see design §3):

- On login: generate a random 32-byte session token (do not store raw token — store `SHA-256(token)` in `sessions.token_hash`). Generate a separate random 32-byte CSRF token. Insert a row into `sessions` with `csrf_token`, `token_hash`, `user_id`, `expires_at = now + 30 days`.
- Issue two cookies:
  - `session=<raw_token>; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=2592000`
  - `csrf=<csrf_token>; Secure; SameSite=Strict; Path=/; Max-Age=2592000` (no HttpOnly — JS must read it)
- On every state-changing request (POST/PUT/DELETE): extract the `session` cookie → hash it → look up `sessions.token_hash`. Then validate `X-CSRF-Token` header equals `sessions.csrf_token`. Reject with 403 if either fails.
- Implement this as an Axum extractor `AuthenticatedUser` that all protected handlers use.
- On logout: delete the `sessions` row, set both cookies to `Max-Age=0`.
- Password hashing: use `argon2` crate with `argon2id` variant. Never compare plaintext passwords. Use `PasswordHash::verify_password`.

**Phase 1 users:** Two seeded users from the migration. Passwords set via a CLI helper: `buywise-cli set-password <email>` (prompts for password, hashes, updates the DB row). No public registration.

**Files / Areas Touched:**
- `crates/api/src/auth.rs` (create)
- `crates/api/src/middleware.rs` (create: `AuthenticatedUser` extractor)
- `crates/api/src/main.rs` (add routes)

**Acceptance Criteria:**
- [ ] `POST /api/auth/login` with correct credentials → 200, both cookies set
- [ ] `POST /api/auth/login` with wrong password → 401
- [ ] `GET /api/auth/me` with valid session cookie → 200 with user JSON
- [ ] `GET /api/auth/me` without session cookie → 401
- [ ] `POST` to any protected endpoint without `X-CSRF-Token` → 403
- [ ] `POST` with wrong CSRF token → 403
- [ ] Session `token_hash` in DB is SHA-256 of the cookie value, not the raw token
- [ ] Logout clears both cookies and deletes the session row

**Testing Expectations:**
- Integration tests using Axum's test client: login → get /me → logout → get /me returns 401
- Test CSRF validation: login, then POST without header, then POST with wrong header
- Test expired sessions (set `expires_at` in the past, assert 401)

---

### P1-T3: List CRUD endpoints

**Type:** feature

**Description:**

Implement list management routes:

- `GET /api/lists` — return all active lists for the household
- `POST /api/lists` — create a new list
- `GET /api/list/:id` — return full list state (list metadata + the complete set of entries — every status: `active`, `bought`, `skipped`, and `deleted` tombstones — each with its resolution status, and the current revision)
- `PATCH /api/list/:id` — rename or archive a list
- `GET /api/items` — return the full canonical catalogue (for client-side resolution and category assignment)

All routes require `AuthenticatedUser`. Household membership is checked: user must be a member of the list's household.

The `GET /api/list/:id` response includes `current_revision` at the top level so the client knows the baseline for its next reconnect.

`GET /api/list/:id` doubles as the offline-sync snapshot serialiser: it carries the COMPLETE list state (every entry status, not active-only) and includes `list_id` on each entry, mirroring the `POST /api/mutations` / SSE `mutation` `entity` shape. This lets a client use it (and the SSE `full_snapshot` it backs) as a faithful full replacement that reconciles bought/skipped/deleted rows, not just active ones (issue #62). The read UI buckets entries by `status` client-side (active list vs the "In basket" section), so carrying all statuses serves both consumers from one payload.

**Files / Areas Touched:**
- `crates/api/src/lists.rs` (create)
- `crates/api/src/main.rs` (add routes)

**Acceptance Criteria:**
- [ ] `GET /api/lists` returns only lists belonging to the authenticated user's household
- [ ] `GET /api/list/:id` response includes `current_revision` field
- [ ] `GET /api/list/:id` response includes all `list_entries` (every status, not active-only) with their `list_id`, `resolution_status`, and `effective_constraint_hash`
- [ ] A user from household A cannot read a list belonging to household B → 403
- [ ] `GET /api/items` returns all canonical items with their aliases and constraints

**Testing Expectations:**
- Integration tests: create list, get list, verify `current_revision = 0`
- Test household isolation: two test households, assert cross-access is rejected

---

### P1-T4: Mutation endpoint

**Type:** feature

**Description:**

Implement `POST /api/mutations`. This is the primary write path for all list changes.

**Request body** (see design §11 mutation schema):

```json
{
  "client_mutation_id": "uuid-v4",
  "device_id": "stable-device-uuid",
  "list_id": "...",
  "base_revision": 47,
  "entity_type": "list_entry",
  "entity_id": "entry-uuid",
  "operation_type": "mark_bought",
  "payload": { "bought_at": "2026-05-28T14:32:00Z" },
  "created_at_client": "..."
}
```

**Response:**

```json
{
  "client_mutation_id": "...",
  "status": "accepted",
  "server_revision": 48,
  "entity": { ... }
}
```

**Implementation:**

1. Check `client_mutation_id` uniqueness — if already in `mutations` table, return `mutations.response_json` directly (idempotency). The response body is stored on first acceptance so replays are byte-identical, not reconstructed.
2. Verify the user is a member of the list's household.
3. Apply the mutation to the relevant table (`list_entries`, etc.) inside a single transaction.
4. Increment `shopping_lists.current_revision` atomically within the same transaction.
5. Insert a row into `mutations` with `server_revision = new current_revision`, `status = 'accepted'`.
6. After committing, broadcast the mutation via SSE (see P1-T5). Broadcasting is outside the transaction.
7. Return the response.

**Conflict rules** (see design §11):

- `add_item`: attempt insert into `list_entries`. On unique index conflict (`uq_list_entries_active_dedup` fires): increment `quantity` and return `status: 'merged'`. This applies only to resolved entries with the same `effective_constraint_hash`. Unresolved entries (which have `effective_constraint_hash = 'unresolved:<entry_id>'`) are never merged — the sentinel makes each row unique.
- `delete_item` conflicts with a concurrent `mark_bought`: keep the delete; insert an event for the bought action in `list_entry_events`.
- `set_quantity` (issue #60): set the entry's `quantity` to an **absolute** value (payload `{ "quantity": <int in 1..=9999> }`), last-write-wins. Absolute (not a delta) so retries/LWW are idempotent: a replayed or stale-queued op can never double-count. The server rejects a missing/non-integer payload or a value outside `1..=9999` with 422 (reject, not clamp); `MAX_QUANTITY = 9999` is mirrored client-side. Delete wins (a deleted entry is not resurrected). This is the syncable representation of the list UI's +/- controls; the client disables the `-` button at quantity 1 and `+` at 9999. (Distinct from `add_item`'s merge path, which increments `quantity` only when a duplicate resolved item collides on the dedup index.)
- Last-write-wins for all field updates: the server always accepts the mutation and assigns a new revision.

**`effective_constraint_hash` for new entries:** When `operation_type = 'add_item'` and the entry has no constraints, the server sets `effective_constraint_hash = 'unresolved:<entry_id>'` if `canonical_item_id IS NULL`, or `'default'` if resolved with empty constraints. If constraints are present, the server computes SHA-256 of sorted-key JSON (see design §7 hash rule).

**Files / Areas Touched:**
- `crates/api/src/mutations.rs` (create)
- `crates/api/src/main.rs` (add route)
- `crates/core/src/constraint_hash.rs` (create: SHA-256 hash computation)

**Acceptance Criteria:**
- [ ] Submitting the same `client_mutation_id` twice returns the byte-identical response (replayed from `mutations.response_json`)
- [ ] `shopping_lists.current_revision` increments by 1 per accepted mutation
- [ ] `server_revision` in the response equals the new `current_revision`
- [ ] `add_item` for a duplicate resolved item increments `quantity` and returns `status: 'merged'`
- [ ] `add_item` for an unresolved item always creates a new row (sentinel hash prevents merge)
- [ ] `effective_constraint_hash` for a no-constraint entry is `'default'`, not empty string or NULL

**Testing Expectations:**
- Integration tests: submit 5 mutations, verify `current_revision = 5`
- Test idempotency: submit same mutation twice, assert DB has exactly one mutations row
- Test dedup: add same resolved item twice, assert `quantity = 2` and `merged` status
- Test unresolved dedup: add same unresolved item twice, assert two rows (no merge)

---

### P1-T5: SSE stream

**Type:** feature

**Description:**

Implement `GET /api/sse/:list_id`. This endpoint keeps a persistent HTTP connection open and pushes mutation events to connected clients.

**Event format:**

```
event: mutation
data: {"server_revision":48,"entity":{"id":"...","status":"bought",...}}

event: heartbeat
data: {}
```

**Implementation:**

- Use Axum's `axum::response::sse::Sse` with a `tokio::sync::broadcast::Sender<SseEvent>` stored in `AppState`.
- One broadcast channel per list (keyed by `list_id`). Create on first subscriber, keep alive while subscribers exist. A `DashMap<String, broadcast::Sender<SseEvent>>` in `AppState` works well.
- When a mutation is applied (P1-T4), after the transaction commits, send the mutation event to the channel for that `list_id`.
- Send a heartbeat every 30 seconds to keep proxies from closing idle connections.
- On reconnect: the client sends query param `?since=<last_revision>`. The server queries `mutations WHERE list_id = ? AND server_revision > ?` and streams them as backfill before switching to live events. If `since` is below the retention floor (`current_revision - 10000` or 90 days), return `event: full_snapshot` with the complete list state instead.

**Retention check logic:**

```rust
let retention_floor = (current_revision - 10_000).max(0);
let oldest_retained = query_oldest_mutation_revision(list_id, pool).await?;
if client_revision < oldest_retained || client_revision < retention_floor {
    // send full snapshot, not incremental replay
}
```

**Files / Areas Touched:**
- `crates/api/src/sse.rs` (create)
- `crates/api/src/state.rs` (create: `AppState` with broadcast channels map)
- `crates/api/src/main.rs` (wire state, add route)

**Acceptance Criteria:**
- [ ] Two clients connected to the same list both receive the mutation event after one submits a mutation
- [ ] A client reconnecting with `?since=47` receives all mutations with `server_revision > 47`
- [ ] A client reconnecting with a revision below the retention floor receives a `full_snapshot` event
- [ ] Heartbeat is sent every 30 seconds
- [ ] Disconnecting one client does not affect the other

**Testing Expectations:**
- Integration test: connect two SSE clients, submit a mutation via POST, assert both receive the event
- Test backfill: insert 5 mutations directly, connect with `?since=2`, assert 3 events received
- Test snapshot fallback: set retention floor above client revision, assert `full_snapshot` event

---

### P1-T6: Axum server wiring and static file serving

**Type:** feature

**Description:**

Wire the full Axum application in `crates/api/src/main.rs`. Serve the SvelteKit static build alongside the API on the same port (see design §5: `tower-http ServeDir`).

**Router structure:**

```
POST   /api/auth/login
POST   /api/auth/logout
GET    /api/auth/me
GET    /api/lists
POST   /api/lists
GET    /api/list/:id
PATCH  /api/list/:id
GET    /api/items
POST   /api/mutations
GET    /api/sse/:list_id
POST   /api/optimise           (added in Phase 4, stub for now)
GET    /* → ServeDir("frontend/build")   SPA fallback
```

**AppState:**

```rust
pub struct AppState {
    pub db: SqlitePool,
    pub sse_channels: DashMap<String, broadcast::Sender<SseEvent>>,
}
```

Configure SQLite connection pool:
- WAL mode: `PRAGMA journal_mode=WAL`
- Busy timeout: `PRAGMA busy_timeout=5000` (5 seconds)
- Foreign keys: `PRAGMA foreign_keys=ON`
- Max connections: 8 (sufficient for 2 users + background jobs)

`RUST_LOG` controlled logging via `tracing-subscriber`. Bind on `0.0.0.0:3000` by default; override via `PORT` env var.

**Files / Areas Touched:**
- `crates/api/src/main.rs`
- `crates/api/src/state.rs`

**Acceptance Criteria:**
- [ ] `cargo run -p buywise-api` starts without error with an existing SQLite file
- [ ] `GET /` serves the SvelteKit app shell when `frontend/build/` exists; production deploys install that build before starting the service (see `deploy/README.md`)
- [ ] All API routes respond with correct HTTP status codes
- [ ] SQLite connection pool has WAL mode, busy timeout, and foreign keys enabled

**Testing Expectations:**
- Integration test: start server with `SocketAddr::from(([127,0,0,1], 0))` (port 0), run a smoke test against each route

---

### P1-T7: Hetzner deployment

**Type:** chore

**Description:**

Create the deployment configuration for the Hetzner CAX11 ARM server (see design §5 and §6).

**Files to create:**

`deploy/systemd/buywise.service`:
```ini
[Unit]
Description=BuyWise grocery optimiser
After=network.target

[Service]
Type=simple
User=buywise
WorkingDirectory=/var/lib/buywise
ExecStart=/usr/local/bin/buywise-api
Restart=on-failure
RestartSec=5
EnvironmentFile=/etc/buywise/buywise.env
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

`deploy/systemd/litestream.service`:
```ini
[Unit]
Description=Litestream replication for BuyWise
After=network.target

[Service]
Type=simple
User=buywise
ExecStart=/usr/local/bin/litestream replicate -config /etc/litestream.yml
Restart=on-failure
RestartSec=5
EnvironmentFile=/etc/buywise/buywise.env

[Install]
WantedBy=multi-user.target
```

`deploy/litestream.yml` — fill in the Cloudflare R2 bucket and endpoint (from the existing skeleton file, replacing placeholder values with correct R2 endpoint format and the correct DB path `/var/lib/buywise/buywise.db`).

`deploy/setup.sh` — idempotent setup script for a fresh Hetzner CAX11 Ubuntu ARM instance:
- Create `buywise` user
- Create `/var/lib/buywise/` directory with correct ownership
- Install systemd units
- Print instructions for setting env vars in `/etc/buywise/buywise.env`

`deploy/README.md` — steps to deploy: build ARM binary (`cargo build --release --target aarch64-unknown-linux-gnu`), copy binary, run `setup.sh`, start services.

**Files / Areas Touched:**
- `deploy/systemd/buywise.service` (create)
- `deploy/systemd/litestream.service` (create)
- `deploy/litestream.yml` (update existing skeleton)
- `deploy/setup.sh` (create)
- `deploy/README.md` (create)

**Acceptance Criteria:**
- [ ] `systemctl status buywise` shows active on a test VM
- [ ] Litestream starts and writes to the configured R2 bucket (verify in R2 dashboard)
- [ ] After `systemctl restart buywise`, the DB is intact (WAL checkpointed, Litestream replicated)
- [ ] `GET /api/auth/me` responds from the deployed server

**Testing Expectations:**
- Manual smoke test on the Hetzner server; no automated tests for this task

---

## Phase 2 — SvelteKit PWA

**Gate:** Both users can manage their shopping list from a mobile browser. Check-off works offline in-store. Mutations sync on reconnect.

---

### P2-T1: SvelteKit scaffold and Dexie schema

**Type:** chore

**Description:**

Scaffold the SvelteKit PWA in `frontend/`. Use `npm create svelte@latest frontend` with the following choices: TypeScript, no ESLint/Prettier (add manually), adapter-static (for SPA mode). See design §5: `adapter-static` in SPA mode — no SvelteKit server routes in production.

**Dexie schema** (IndexedDB via Dexie.js):

```typescript
// frontend/src/lib/db/dexie.ts
import Dexie, { type Table } from 'dexie';

export interface LocalListEntry {
  id: string;                    // UUID v4; matches server entry.id
  list_id: string;
  canonical_item_id?: string;
  resolution_status: 'unresolved' | 'resolved' | 'ambiguous';
  raw_text: string;
  display_name: string;
  quantity: number;
  status: 'active' | 'bought' | 'skipped' | 'deleted';
  bought_by?: string;
  bought_at?: string;
  category_id?: string;
  last_updated_revision: number;
  updated_at: string;
}

export interface LocalMutation {
  id?: number;                   // Dexie auto-increment PK
  client_mutation_id: string;    // UUID v4; idempotency key
  device_id: string;
  list_id: string;
  entity_type: string;
  entity_id: string;
  operation_type: string;
  payload: object;
  base_revision: number;
  created_at_client: string;
  status: 'pending' | 'sent' | 'confirmed' | 'failed';
  retry_count: number;
}

export interface LocalListMeta {
  id: string;                    // list_id
  name: string;
  current_revision: number;      // last known server revision
  last_synced_at?: string;
}

export class BuyWiseDB extends Dexie {
  list_entries!: Table<LocalListEntry, string>;
  mutation_queue!: Table<LocalMutation, number>;
  list_meta!: Table<LocalListMeta, string>;

  constructor() {
    super('buywise');
    this.version(1).stores({
      list_entries: 'id, list_id, status, canonical_item_id, last_updated_revision',
      mutation_queue: '++id, list_id, status, client_mutation_id',
      list_meta: 'id',
    });
  }
}

export const db = new BuyWiseDB();
```

**Package dependencies to add:**
- `dexie` — IndexedDB wrapper
- `dexie-react-hooks` is NOT needed (Svelte uses stores)
- `@vite-pwa/sveltekit` — PWA plugin for Vite
- `workbox-window` — for service worker communication

**vite.config.ts**: add `@vite-pwa/sveltekit` plugin with `registerType: 'autoUpdate'`, `manifest` containing `name: 'BuyWise'`, `short_name: 'BuyWise'`, `theme_color`, `icons`.

**svelte.config.js**: use `adapter-static` with `fallback: '200.html'` for SPA mode.

**Files / Areas Touched:**
- `frontend/` (scaffold via `npm create svelte`)
- `frontend/src/lib/db/dexie.ts` (create)
- `frontend/vite.config.ts` (update)
- `frontend/svelte.config.js` (update)
- `frontend/package.json`

**Acceptance Criteria:**
- [ ] `npm run build` in `frontend/` produces `frontend/build/` with static files
- [ ] Serving `frontend/build/` from the Axum binary via `ServeDir` works
- [ ] `new BuyWiseDB()` creates all three stores without errors in a browser console
- [ ] The PWA manifest is present and valid (check via Chrome DevTools → Application)

**Testing Expectations:**
- Manual test: open in Chrome, check IndexedDB has the three Dexie tables

---

### P2-T2: Service worker — app shell offline

**Type:** feature

**Description:**

Configure the service worker via `@vite-pwa/sveltekit` to cache the SvelteKit app shell. The app shell must load from cache when offline.

Use Workbox's `CacheFirst` strategy for static assets and `NetworkFirst` with a cache fallback for API reads (only `GET /api/list/:id` and `GET /api/items` need caching — other API calls are mutations or streaming and should not be cached).

**Precaching manifest**: include `index.html`, all JS/CSS chunks from the build. Do not precache API responses — those are handled by runtime caching strategies.

**Offline fallback**: when a network request to an uncached resource fails offline, the service worker returns the cached app shell (`/index.html`), allowing the SPA router to handle it client-side.

**Files / Areas Touched:**
- `frontend/vite.config.ts` (PWA plugin workbox config)
- `frontend/src/service-worker.ts` (if using a custom SW; otherwise relying on the generated one)

**Acceptance Criteria:**
- [ ] With Chrome DevTools → Network → Offline checked, the app shell loads from cache
- [ ] With offline mode on, navigating to `/list/:id` shows the cached list (not a blank page)
- [ ] API GET responses for `/api/list/:id` are served from cache when offline

**Testing Expectations:**
- Manual test: load app online, go offline, reload — app shell should render

---

### P2-T3: List UI — add, check off, skip, delete

**Type:** feature

**Description:**

Build the core list management UI in SvelteKit. The list view must work identically online and offline — mutations go into Dexie's `mutation_queue` immediately; the sync loop (P2-T4) drains them when online.

**Components to build:**

- `frontend/src/routes/(app)/list/[id]/+page.svelte` — main list view
- `frontend/src/lib/components/ListItem.svelte` — individual item row
- `frontend/src/lib/components/AddItemInput.svelte` — text input with debounced submission

**Mutation creation pattern:** every user action creates a `LocalMutation` record in Dexie and optimistically updates the `LocalListEntry` in Dexie immediately. The sync loop sends it to the server asynchronously. This means the UI never waits for the server.

**Operations exposed in the UI:**
- Add item (raw text input; quantity defaults to 1)
- Increment / decrement quantity
- Check off (`mark_bought`)
- Mark skipped
- Delete item (swipe-to-delete or long-press context menu)
- Pull-to-refresh (force sync)

**Bought/skipped items:** show below the active items in a collapsed section "In basket (N)". Deleted items are not shown.

**Files / Areas Touched:**
- `frontend/src/routes/(app)/list/[id]/+page.svelte`
- `frontend/src/lib/components/ListItem.svelte`
- `frontend/src/lib/components/AddItemInput.svelte`
- `frontend/src/lib/stores/list.ts` (Svelte store wrapping Dexie live queries)

**Acceptance Criteria:**
- [ ] Adding an item creates a Dexie `mutation_queue` entry immediately
- [ ] Checking off an item updates the local Dexie record immediately (before server confirmation)
- [ ] With offline mode on, all operations work and queue up in `mutation_queue`
- [ ] Bought items collapse into the "In basket" section
- [ ] Item count in the header updates reactively

**Testing Expectations:**
- Component tests using Vitest + Svelte Testing Library: add item, assert Dexie has pending mutation
- Manual offline test

---

### P2-T4: Sync loop — drain queue and apply SSE updates

**Type:** feature

**Description:**

Implement the client-side sync engine in `frontend/src/lib/sync/`.

**Mutation queue drain** (reference: Shoppy's `SyncQueue.ts`, see design §15):

```typescript
// frontend/src/lib/sync/SyncQueue.ts
const MAX_RETRIES = 3;

// On reconnect (navigator.onLine === true and SSE reconnects):
// 1. Load all mutations with status='pending' from Dexie, ordered by id (creation order)
// 2. For each mutation:
//    a. POST to /api/mutations
//    b. On success (status: 'accepted' | 'merged'):
//       - Update LocalListEntry with the server's returned entity
//       - Update LocalListMeta.current_revision
//       - Mark mutation as 'confirmed'
//    c. On network error: retry up to MAX_RETRIES times with exponential backoff
//    d. On 4xx (rejected): mark mutation as 'failed', surface error to user
// 3. After draining, subscribe to SSE stream with ?since=<current_revision>
```

**SSE update application:**

```typescript
// frontend/src/lib/sync/SseClient.ts
// Connect to GET /api/sse/:list_id?since=<revision>
// On 'mutation' event: apply the entity update to Dexie (upsert LocalListEntry)
// On 'full_snapshot' event: replace all list_entries for this list_id in Dexie
// On 'heartbeat': update last_seen timestamp
// On error / close: reconnect with exponential backoff (1s → 2s → 4s → 8s → cap 30s)
```

**Device ID:** generate a UUID v4 on first load, persist in `localStorage`. Use as `device_id` in all mutations.

**Conflict handling in the client:** the client is "last-write-wins" by design. When the server returns an entity in the mutation response, the client replaces its local copy with the server's version. There is no client-side conflict resolution — the server is authoritative.

**Files / Areas Touched:**
- `frontend/src/lib/sync/SyncQueue.ts` (create)
- `frontend/src/lib/sync/SseClient.ts` (create)
- `frontend/src/lib/sync/index.ts` (export both, start on mount)

**Acceptance Criteria:**
- [ ] With two tabs open, checking off an item in tab A updates tab B within 1 second (via SSE)
- [ ] Mutations created offline appear in Dexie with `status='pending'`
- [ ] On reconnect, all `status='pending'` mutations drain in order
- [ ] After drain, `LocalListMeta.current_revision` equals the server's `current_revision`
- [ ] After drain, a full `GET /api/list/:id` and the local Dexie state are identical

**Testing Expectations:**
- Integration test (Playwright): two browser contexts, offline one, add item, reconnect, verify item appears in both
- Unit tests: mock SSE events, assert Dexie state transitions correctly

---

### P2-T5: Duplicate warning UX

**Type:** feature

**Description:**

Port Shoppy's duplicate detection UX to Svelte. Phase 2 uses **dictionary-only client-side detection** — no server call (see design §14 Phase 2 note).

When the user types in the add-item input and there is already an active item that matches (via exact, dictionary, fuzzy, or contains), show an inline warning before submission:

```
"Milk" is already on your list.
[Add anyway (+1)] [Merge quantity] [Cancel]
```

**Cascade (client-side only in Phase 2):**
1. Exact match (normalised) → show warning
2. Dictionary match (bundled bilingual dictionary in the SvelteKit app) → show warning
3. Fuzzy match (Levenshtein ≤ 2, minLength ≥ 4) → show warning
4. Contains match (bidirectional, minLength ≥ 5) → show warning

The bundled dictionary is a compiled TypeScript module (import the Shoppy dictionary logic directly, porting it to the new format). No aliases are written to the server in Phase 2; that happens in Phase 3 once the server-side cascade is live.

**"Merge quantity"** action: creates an `add_item` mutation with the existing entry's `entity_id`, causing the server to increment `quantity` (the server's `add_item` dedup logic handles this via the partial unique index — the merge happens server-side). The client optimistically increments the local `quantity`.

**"Add anyway"** action: creates a new `add_item` mutation with a fresh UUID. At the point of client-side addition both entries are `unresolved` (different `entry_id`, different `effective_constraint_hash = 'unresolved:<entry_id>'`), so two rows are created. However, when both resolve to the same canonical item + constraint hash, the resolution conflict handler (see P3-T1) will set `status = 'deleted'` on the losing entry and record a `list_entry_events` row with `event_type = 'merged'`, pointing to the survivor — the two entries effectively collapse into one with incremented quantity. **The acceptance criterion "no merge" applies only at the moment of the initial `add_item`**, not after resolution. The UI should note — at the time the "Add anyway" button is shown — that items may be automatically combined once resolved.

**Files / Areas Touched:**
- `frontend/src/lib/components/DuplicateWarning.svelte` (create)
- `frontend/src/lib/matching/duplicate.ts` (create: port Shoppy's DuplicateDetector)
- `frontend/src/lib/matching/dictionary.ts` (create: bundled bilingual dictionary)
- `frontend/src/lib/components/AddItemInput.svelte` (update to show warning)

**Acceptance Criteria:**
- [ ] Adding "leche" when "Milk" is already active → warning appears
- [ ] "Add anyway" creates a new entry server-side at the time of insertion (both entries are unresolved, so no dedup index conflict occurs)
- [ ] "Merge quantity" increments quantity on the existing entry
- [ ] No server call is made for the duplicate check itself
- [ ] Warnings do not fire for genuinely distinct items ("lactose-free milk" ≠ "milk" if constraints differ)

**Testing Expectations:**
- Component tests: add "leche" to a list containing "Milk", assert warning renders
- Unit tests for `duplicate.ts`: port `DuplicateDetector.test.ts` from Shoppy

---

### P2-T6: Shopping mode

**Type:** feature

**Description:**

Build the shopping mode view at `frontend/src/routes/(app)/list/[id]/shopping/+page.svelte`.

**Behaviour:**

- A store-selector component shows the 5 retailers. Selecting one switches the category ordering to that store's `store_category_orders` from the server (cached in Dexie or a store).
- Sections group items by sub-category when one is assigned, ordered by the sub-category's `default_order` within its parent; otherwise they fall back to the parent category. The parent's store `display_order` orders all its sections.
- Items within a section maintain their entry order.
- Each section shows as a collapsible group with its category or sub-category name and remaining item count.
- Checked-off items remain in their section with a strikethrough while that section has active items.
- A progress bar at the top shows `bought_count / active_count`.
- This view is fully offline-capable (all data from Dexie).

**Store category orders:** fetched with the catalogue from `GET /api/items` and stored in Dexie. Store ordering applies to parent categories; sub-categories use their seeded default order. Per-store aisle layouts, a reorder screen, and learned aisle order are out of scope (issue #101); see [design.md](design.md#7-data-model).

**Files / Areas Touched:**
- `frontend/src/routes/(app)/list/[id]/shopping/+page.svelte` (create)
- `frontend/src/lib/components/StoreSelector.svelte` (create)
- `frontend/src/lib/components/CategoryGroup.svelte` (create)
- `frontend/src/lib/components/ProgressBar.svelte` (create)

**Acceptance Criteria:**
- [ ] Selecting Tesco groups items in Tesco's category order
- [ ] Selecting Aldi switches to Aldi's category order
- [ ] Progress bar updates when an item is checked off
- [ ] View works with Dexie offline (no network required)
- [ ] Categories with zero active items are hidden (or shown greyed out — product decision; pick one and document it)

**Testing Expectations:**
- Manual test: add items across multiple categories, enter shopping mode, select stores, verify ordering
- Unit tests: `sortByStoreOrder(items, orders, storeId)` pure function

---

## Phase 3 — Bilingual Resolution Cascade

**Gate:** A new item typed in Spanish resolves to the correct canonical item without user prompting (for unambiguous terms). Ambiguous terms show a clarification prompt. Confirmed mappings persist and are used on next entry.

**AI status:** P3-T2 (the LLM fallback) is implemented as an optional, off-by-default extra. It is not part of this gate and is not committed work for the phase; with the switch unset, an unmatched term stays `unresolved` for user clarification, which is the shipped default. The embedding shortlist is likewise offline by default and lives outside the phase plan.

---

### P3-T1: Server-side resolution cascade — alias and fuzzy steps

**Type:** feature

**Description:**

Extend the mutation handler to run the full server-side resolution cascade (steps 1 and 2 from §8) when an `add_item` mutation arrives with `resolution_status = 'unresolved'`.

The cascade is run asynchronously after the mutation is accepted. The result (`SingleMatch`, `MultipleCandidates`, or `NoMatch`) is used to update the `list_entries` row and broadcast an SSE event with the updated resolution status.

**Flow for `add_item` mutation:**

1. Accept and commit the mutation (as in P1-T4) with `resolution_status = 'unresolved'` and `effective_constraint_hash = 'unresolved:<entry_id>'`.
2. Spawn a `tokio::spawn` to run the cascade:
   - Step 1: alias lookup (normalised exact match in `item_aliases`)
   - Step 2: fuzzy match against `canonical_items.name_en`, `canonical_items.name_es`, and all `item_aliases` for this household
3. If `SingleMatch`: open a write transaction and attempt `UPDATE list_entries SET resolution_status='resolved', canonical_item_id=..., effective_constraint_hash=<computed> WHERE id=<this_entry_id>`. The hash must be computed via `compute_constraint_hash(constraints)` — do **not** hardcode `'default'` unless the item genuinely has no constraints. The unique index fires *during* the UPDATE (before it completes) if a conflicting row exists, so the conflict must be caught as a constraint violation error from the database, not checked afterwards.
   - **Dedup conflict on resolution:** the unique index fires *before* the UPDATE succeeds — catch the constraint violation, then in the same transaction:
     1. Query the surviving active row (same `list_id`, `canonical_item_id`, `effective_constraint_hash`).
     2. Increment `quantity` on the survivor.
     3. Set `status = 'deleted'` on the current entry (`'merged'` is not a valid status per the schema CHECK constraint).
     4. Insert a `list_entry_events` row on the current entry with `event_type = 'merged'` and `payload = { "merged_into": "<survivor_entry_id>" }` — this preserves the audit trail.
     5. Broadcast two SSE events: the current entry's deletion and the survivor's updated quantity.
   - This covers the case where two "leche" entries are added offline and both try to resolve to the same canonical item + hash.
4. If `MultipleCandidates`: UPDATE `list_entries SET resolution_status='ambiguous', resolution_candidates_json=[...ids]`, broadcast SSE update. No dedup conflict possible (canonical_item_id stays NULL).
5. If `NoMatch`: leave as `unresolved` (Phase 3 will add LLM fallback in P3-T2).

The cascade must apply the normaliser from P0-T5 to the `raw_text` before matching.

**Files / Areas Touched:**
- `crates/api/src/resolution.rs` (create: async resolution task)
- `crates/api/src/mutations.rs` (update: spawn resolution task after commit)
- `crates/core/src/matching.rs` (already has the cascade logic from P0-T6)

**Acceptance Criteria:**
- [ ] Adding "leche" → after a short delay, entry transitions to `resolution_status='resolved'` with correct `canonical_item_id`
- [ ] Adding "cream" → entry transitions to `resolution_status='ambiguous'` with candidate IDs
- [ ] Adding "xyzqrst_unknown" → entry stays `resolution_status='unresolved'`
- [ ] SSE event is broadcast for each resolution outcome
- [ ] The mutation itself is accepted immediately (before resolution completes) — UI should not block

**Testing Expectations:**
- Integration tests: submit `add_item` mutation, poll DB for resolution update, assert status changes within 500ms
- Test that resolution failure (DB error in the spawned task) does not affect the mutation's committed state

---

### P3-T2: LLM fallback for no_match

**Status:** Implemented as an optional, off-by-default extra (`LLM_FALLBACK_ENABLED`, `crates/api/src/llm.rs`). It is not a requirement of Phase 3: the phase gate and every feature above it are met without it, and the shipped default leaves an unmatched term `unresolved` for user clarification. The scope below is kept as written for reference.

**Type:** feature

**Description:**

Add step 3 of the resolution cascade: when step 2 returns `NoMatch`, make a cheap LLM API call to find the most plausible canonical item.

**LLM call design:**

- Model and API key are env vars: `LLM_PROVIDER`, `LLM_API_KEY`, `LLM_MODEL`. Use a cheap, fast model — consult current provider pricing at implementation time; do not bake in specific model names as they change frequently.
- Prompt: provide the `raw_text`, the list of `canonical_items.name_en` for the household, and ask: "Which of these grocery items is this most likely referring to? Return only the item name from the list, or 'none' if there is no plausible match."
- Parse the response: if it matches exactly one canonical item name → `SingleMatch`; otherwise → fall through to user clarification (step 4).
- Commit the result to `item_aliases` with `source = 'llm'`.
- If the LLM API call fails (timeout, rate limit, error): log, do not retry immediately, leave the entry as `unresolved`. The user will see it flagged for clarification.

**`multiple_candidates` does NOT go through LLM** (see design §8): when step 2 returns `MultipleCandidates`, skip directly to user clarification.

**API cost control:** track LLM calls in `api_usage_events` (add `source = 'llm'`). Gate LLM calls behind a configurable flag `LLM_FALLBACK_ENABLED=true` (default `false` in dev).

**Files / Areas Touched:**
- `crates/api/src/llm.rs` (create: LLM client abstraction)
- `crates/api/src/resolution.rs` (update: add step 3 after NoMatch)

**Acceptance Criteria:**
- [ ] A genuinely unknown but plausible term (e.g. "zumo de manzana" — apple juice) resolves via LLM to the canonical item
- [ ] The resolved alias is persisted to `item_aliases` with `source='llm'`
- [ ] Second entry of the same term resolves via step 1 (alias lookup) without LLM call
- [ ] LLM API failure leaves the entry `unresolved` (no panic, no retry storm)
- [ ] `LLM_FALLBACK_ENABLED=false` skips the LLM call entirely

**Testing Expectations:**
- Unit tests with mocked HTTP client: test happy path, API error, "none" response
- Integration test: verify alias is persisted after LLM success

---

### P3-T3: Ambiguity confirmation flow in PWA

**Type:** feature

**Description:**

When a list entry has `resolution_status = 'ambiguous'`, show a disambiguation UI in the list view.

**UI design:**

The item row shows a "? Tap to clarify" indicator. Tapping opens a modal:

```
What did you mean by "cream"?
  ○ Single cream
  ○ Double cream
  ○ Sour cream
  ○ Ice cream
  ○ Face cream
[Confirm] [Keep as-is]
```

"Keep as-is" dismisses without resolving. "Confirm" sends a `resolve_alias` mutation:

```json
{
  "operation_type": "resolve_alias",
  "entity_id": "<entry_id>",
  "payload": {
    "chosen_canonical_item_id": "<id>",
    "raw_text": "cream"
  }
}
```

The server handles this mutation by updating `list_entries` to `resolution_status='resolved'`, setting `canonical_item_id`, and persisting to `item_aliases`.

**Files / Areas Touched:**
- `frontend/src/lib/components/AmbiguityModal.svelte` (create)
- `frontend/src/lib/components/ListItem.svelte` (update: show disambiguation indicator)
- `crates/api/src/mutations.rs` (update: handle `resolve_alias` operation)

**Acceptance Criteria:**
- [ ] An `ambiguous` entry shows a "?" indicator in the list
- [ ] Tapping opens modal with exactly the candidate items
- [ ] Confirming a choice transitions entry to `resolved`
- [ ] "Keep as-is" leaves the entry `ambiguous` (does not spam the server)
- [ ] The confirmed mapping is persisted in `item_aliases` with `source='accepted_match'`

**Testing Expectations:**
- Component tests: render `AmbiguityModal` with mock candidates, click confirm, assert mutation emitted
- Integration test: resolve_alias mutation → assert item_aliases row created

---

### P3-T4: item_aliases feedback persistence

**Type:** feature

**Description:**

Ensure all confirmed raw-text→canonical mappings are persisted to `item_aliases` consistently across all resolution paths. This task audits and tests the persistence logic added in P3-T1 through P3-T3.

Rules:
- Step 1 (alias lookup): no new alias needed — it was already there.
- Step 2 (fuzzy match → SingleMatch): persist the `raw_text` (normalised) as a new alias with `source='accepted_match'`.
- Step 3 (LLM → SingleMatch): persist with `source='llm'`.
- Step 4 (user confirmation via `resolve_alias`): persist with `source='accepted_match'`.

Dedup: the unique index on `(household_id, lower(alias), language)` handles the case where the same alias is confirmed multiple times.

Write a regression test that exercises all four paths and verifies the alias table state after each.

**Files / Areas Touched:**
- `crates/api/src/resolution.rs` (audit and fill gaps)
- `crates/core/src/matching.rs` (ensure `commit_alias` helper exists)

**Acceptance Criteria:**
- [ ] After each resolution path, the correct alias row exists in `item_aliases`
- [ ] Re-adding the same term resolves via step 1 (alias lookup) without hitting fuzzy/LLM
- [ ] Language is correctly inferred (`en` for English terms, `es` for Spanish terms; default `en` if unclear)

**Testing Expectations:**
- Regression test suite: one test per resolution path verifying alias persistence

---

## Phase 4 — Optimiser UI

**Gate:** The household can see four basket plans and a plain-language recommendation before leaving for the shops. Convenience threshold is user-configurable.

---

### P4-T1: POST /api/optimise endpoint

**Type:** feature

**Description:**

Expose the optimiser from P0-T10 as an HTTP endpoint.

**Request:**

```json
{
  "list_id": "...",
  "convenience_threshold_2": 1000,   // cents; optional, use household default if absent
  "convenience_threshold_3": 2500
}
```

**Response:**

```json
{
  "run_id": "...",
  "best_single": { ... },
  "best_two_store": { ... },
  "theoretical_minimum": { ... },
  "recommended": { ... },
  "unresolved_items": [ ... ],
  "no_valid_offer_items": [ ... ],
  "plain_language_explanation": "...",
  "offer_snapshot_date": "..."
}
```

Persist a `recommendation_runs` row. Return the `run_id` so the client can correlate with savings events later.

**Files / Areas Touched:**
- `crates/api/src/optimise.rs` (create)
- `crates/api/src/main.rs` (add route)

**Acceptance Criteria:**
- [ ] `POST /api/optimise` with a valid list returns four plans
- [ ] `recommendation_runs` row is persisted
- [ ] `unresolved_items` lists items with `resolution_status != 'resolved'`
- [ ] `plain_language_explanation` mentions which stores are cheaper for which categories

**Testing Expectations:**
- Integration test: seed fixture data, POST /api/optimise, assert response structure
- Test that `unresolved_items` is populated correctly when some entries are ambiguous

---

### P4-T2: Convenience threshold configuration endpoint

**Type:** feature

**Description:**

Allow the household to configure convenience thresholds via a settings endpoint.

- `GET /api/settings` — return household settings including `convenience_threshold_2_cents` and `convenience_threshold_3_cents`
- `PATCH /api/settings` — update thresholds

Store in a new `household_settings` table (add via a new migration `005_household_settings.sql`):

```sql
CREATE TABLE household_settings (
    household_id TEXT PRIMARY KEY REFERENCES households(id),
    convenience_threshold_2_cents INTEGER NOT NULL DEFAULT 1000,
    convenience_threshold_3_cents INTEGER NOT NULL DEFAULT 2500,
    updated_at TEXT NOT NULL
);
```

Seed one row per household when the household is created.

**Files / Areas Touched:**
- `migrations/005_household_settings.sql` (create)
- `crates/api/src/settings.rs` (create)
- `crates/api/src/main.rs` (add routes)

**Acceptance Criteria:**
- [ ] `GET /api/settings` returns the current thresholds
- [ ] `PATCH /api/settings` with new values updates them
- [ ] `POST /api/optimise` without explicit thresholds uses the household's stored values
- [ ] Default is €10 (1000 cents) for threshold_2 and €25 (2500 cents) for threshold_3

**Testing Expectations:**
- Integration test: update thresholds, run optimiser, verify recommendation changes at the boundary

---

### P4-T3: Basket plan display in PWA

**Type:** feature

**Description:**

Build the optimisation view at `frontend/src/routes/(app)/list/[id]/optimise/+page.svelte`.

**UI components:**

- An "Optimise basket" button on the list view triggers a `POST /api/optimise` call.
- Loading state while the server computes plans.
- Plan cards: one card per plan showing store(s), total cost, saving vs best single. The recommended plan is visually highlighted.
- Plain-language explanation section below the cards.
- "Unresolved items" section listing items excluded from the totals, with a link to clarify them.
- Confidence indicators on items with `freshness_confidence = 'low'` or `'unknown'` (see design §12).
- The Dunnes voucher flag from the optimiser (see P0-T10) displayed inline.

**Files / Areas Touched:**
- `frontend/src/routes/(app)/list/[id]/optimise/+page.svelte` (create)
- `frontend/src/lib/components/PlanCard.svelte` (create)
- `frontend/src/lib/components/ConfidenceIndicator.svelte` (create)

**Acceptance Criteria:**
- [ ] Four plan cards render with correct totals
- [ ] Recommended plan is visually distinct
- [ ] Items with `freshness_confidence='unknown'` have a visible indicator
- [ ] Unresolved items are listed separately with a "clarify" action

**Testing Expectations:**
- Component tests: render `PlanCard` with mock plan data, assert total displays correctly
- Manual end-to-end test: add items, optimise, verify plan display

---

## Phase 5 — Household Learning and Savings Ledger

**Gate:** The household can review their monthly savings and see which suggestions were followed.

---

### P5-T1: match_feedback and product_matches write paths

**Type:** feature

**Description:**

The `product_matches` table already exists (from P0-T2). Phase 5 adds the UI flow for users to accept or reject specific product matches from the optimiser plan view.

Add accept/reject buttons to each `PlanItem` in the plan display. These POST to a new endpoint:

`POST /api/match-feedback`:

```json
{
  "canonical_item_id": "...",
  "retailer_product_id": "...",
  "action": "accept"   // or "reject" or "prefer"
}
```

Server: upsert `product_matches` (update `status` in place) and insert `match_feedback` row.

**Files / Areas Touched:**
- `crates/api/src/match_feedback.rs` (create)
- `frontend/src/lib/components/PlanItem.svelte` (create or update)

**Acceptance Criteria:**
- [ ] Accepting a match creates `product_matches` with `status='accepted'`
- [ ] Rejecting updates to `status='rejected'`
- [ ] `match_feedback` row is always inserted (history is preserved)
- [ ] `UNIQUE(household_id, canonical_item_id, retailer_product_id)` is respected — only one preference row per triple

**Testing Expectations:**
- Integration test: accept, then reject same match — assert one `product_matches` row with `status='rejected'` and two `match_feedback` rows

---

### P5-T2: Regression test — rejected matches excluded from plans after Phase 5 write path

**Type:** test / regression

**Description:**

The optimiser already loads only `status IN ('accepted', 'suggested')` candidates (implemented in P0-T10), so rejected products are excluded from the start. This task exists to verify that exclusion still holds end-to-end after the Phase 5 `match_feedback` write path (P5-T1) is wired up — specifically, that rejecting a match via the API immediately removes it from subsequent optimiser runs for that household.

**Files / Areas Touched:**
- `crates/core/src/optimiser.rs` (verify no regression — no code change expected)
- Integration tests only

**Acceptance Criteria:**
- [ ] After calling the match-rejection endpoint (P5-T1), re-running `optimise` for the same household excludes the rejected product
- [ ] If all products for a canonical item are rejected, the item appears in `no_valid_offer_items` (not `unresolved_items`)
- [ ] A different household's rejections do not affect this household's plans (household isolation)

**Testing Expectations:**
- Integration test: seed two products for one item; reject one via the Phase 5 write path; assert only the other appears in plans and the plan total reflects the remaining product's price

---

### P5-T3: savings_events logging on list archive

**Type:** feature

**Description:**

When a shopping list is archived (`PATCH /api/list/:id` with `status: 'archived'`), record a `savings_events` row if a `recommendation_runs` row exists for this list.

`gross_saving_cents` = `baseline_cost_cents - recommended_cost_cents`

`chosen_basket_cost_cents` is left NULL until the user manually enters actual spend (P5-T4).

Also expose `POST /api/list/:id/actual-spend`:

```json
{ "actual_spend_cents": 13240, "travel_cost_cents": 0 }
```

Updates the `savings_events` row and computes `net_saving_cents`.

**Files / Areas Touched:**
- `crates/api/src/lists.rs` (update archive handler)
- `crates/api/src/savings.rs` (create: actual spend endpoint)

**Acceptance Criteria:**
- [ ] Archiving a list with a recommendation run → `savings_events` row created
- [ ] Archiving without a recommendation run → no `savings_events` row (no error)
- [ ] `POST /api/list/:id/actual-spend` updates `chosen_basket_cost_cents` and `net_saving_cents`

**Testing Expectations:**
- Integration test: create recommendation run, archive list, assert savings event row

---

### P5-T4: Savings summary view in PWA

**Type:** feature

**Description:**

Build a savings dashboard at `frontend/src/routes/(app)/savings/+page.svelte`.

**Display:**
- Monthly summary: gross saving, accepted saving, net saving (after travel + subscription allocation)
- Cumulative saving since first use
- A simple table or card list of archived lists with their outcomes
- "Enter actual spend" button for lists without `chosen_basket_cost_cents`

Fetch from a new `GET /api/savings/summary` endpoint:

```json
{
  "monthly": [
    { "month": "2026-05", "gross_cents": 3200, "net_cents": 2800, "lists": 4 }
  ],
  "cumulative_net_cents": 18400
}
```

**Files / Areas Touched:**
- `frontend/src/routes/(app)/savings/+page.svelte` (create)
- `crates/api/src/savings.rs` (add summary endpoint)

**Acceptance Criteria:**
- [ ] Summary shows correct monthly totals aggregated from `savings_events`
- [ ] Cumulative saving is sum of `net_saving_cents` across all non-null rows
- [ ] Lists with `chosen_basket_cost_cents IS NULL` show an "Enter actual spend" prompt

**Testing Expectations:**
- Integration test: seed 3 savings events with known values, call summary endpoint, assert aggregates

---

### Backlog-T1: Full constraint enforcement in the optimiser

**Type:** feature (post-Phase 5)

**Description:**

Extend the optimiser's constraint filtering (step 1c) to honour all `ItemConstraints` fields beyond size and brand. This task exists to close the named gap left by Phase 0.

Constraints to implement:
- `allowed_brands`: accept only products whose brand is in the allow-list
- `excluded_brands`: reject products whose brand appears in the block-list
- `pack_count`: filter by `retailer_products.pack_count` (column already exists in schema)
- `form_factor`: filter by product form (e.g. "can", "bottle", "spray") — **requires a schema migration** to add `form_factor TEXT` to `retailer_products` (the column does not currently exist), and ingest changes to populate it; the `item_constraints.form_factor` column already exists but has no matching column to compare against in `retailer_products`
- `dietary_flags`: filter by dietary attributes (e.g. `["gluten_free", "dairy_free"]`) — **requires a schema migration** to add `dietary_flags TEXT` (JSON array) to `retailer_products`, and ingest changes to extract these from product data; define a standard set of flag strings before implementing
- `substitution_policy = Strict`: do not offer any substitute if the exact matched product has no current offer; show as missing rather than recommending an alternative
- `substitution_policy = Ask`: surface the item to the user for confirmation rather than auto-selecting

Until this task is complete, recommendations for constrained items (nappies size, cans vs bottles, dietary needs) may be incorrect. The schema stores all constraint fields in `item_constraints`; the `retailer_products` side is missing `form_factor` and `dietary_flags` columns.

**Dependencies:** Phase 5 complete; migrations for `retailer_products.form_factor` and `retailer_products.dietary_flags`; ingest adapter changes to populate those fields.

**Canonical flag vocabulary (defined for this task; mirrored in `crates/core/src/domain.rs` as `FORM_FACTORS` / `DIETARY_FLAGS`):**

- `form_factor` — a single lowercase token, compared case-insensitively between `item_constraints.form_factor` and `retailer_products.form_factor`:
  `can`, `bottle`, `carton`, `pouch`, `jar`, `tub`, `bar`, `spray`, `box`, `bag`, `tin`, `tetra`.
- `dietary_flags` — a JSON array of lowercase snake_case tokens; a product qualifies for a constraint only when its set is a **superset** of the required set:
  `gluten_free`, `dairy_free`, `lactose_free`, `vegan`, `vegetarian`, `nut_free`, `organic`, `sugar_free`.

**Enforcement semantics (optimiser `check_constraints`, brand comparisons normalised via `normalize()`):** `allowed_brands` — if set & non-empty the product brand must be in the list (missing brand → reject); `excluded_brands` — product brand in the list → reject; `pack_count` — exact match; `form_factor` — equal (case-insensitive); `dietary_flags` — product set ⊇ required set.

**Substitution policy (classification, surfaced as distinct machine-readable statuses):** `strict` → if the exact (accepted) matched product has no qualifying offer, the item is shown as missing (`NoOfferReason::StrictNoExactOffer`) rather than substituted; `ask` → the item is held back in `OptimiserOutput::needs_confirmation_items` for the household to confirm rather than auto-selected.

**Ingest population:** the BasketWatch/Lidl feeds carry no structured form/dietary fields. Ingest (`crates/ingest/src/attributes.rs`) populates the two columns best-effort from product name + category keywords, preferring any explicit feed-supplied value; an undetectable attribute is written as `NULL` (treated by the optimiser as "unknown", satisfying only an absent constraint). The columns are never silently dropped.

---

## Cross-Cutting Notes

### Migration versioning

All migrations use sqlx's numbered prefix: `001`, `002`, `003`, etc. Apply via `sqlx::migrate!()` in `crates/core/src/db.rs`. The `_sqlx_migrations` table tracks applied versions. Never edit an applied migration; always add a new one.

### Money invariant

Every function that handles money must use `Cents` (`i64`) or `Micros` (`i64`). Never use `f64` for prices. The only float in the codebase is `NormalisedSize` (for dimensional values like grams and millilitres), which never directly represents money.

### `product_offers` append-only rule

No `UPDATE` statements on `product_offers`. If a price needs to be corrected, insert a new row. The optimiser uses `ORDER BY fetched_at DESC LIMIT 1` to get the current offer.

### Freshness confidence is computed, not stored

The fields `source_confidence_at_ingest` (stored) and freshness confidence (computed) are always separate. Never add a `freshness_confidence` column to `product_offers`. Compute it at query time: `current_run_number - offer.source_successful_run_number_at_fetch`.

### CHECK constraints on list_entries

All six CHECK constraints must be in the initial `CREATE TABLE` statement in `migrations/003_phase1_schema.sql`. They cannot be added later via `ALTER TABLE` in SQLite. If a bug is found in them, the fix requires a new migration that recreates the table (copy, drop, rename).

### Dev mode environment variables

Two flags with distinct scopes — do not conflate them:

| Flag | Scope | Effect |
|---|---|---|
| `BUYWISE_NO_NETWORK=1` | Ingest crates | Skips live API calls; reads from `fixtures/offers.json`; still writes `source_runs` and `product_offers` to SQLite. Used by `ingest-basketwatch --dry-run`. CI sets this to prevent accidental API consumption. |
| `BUYWISE_LOCAL_ONLY=1` | CLI `optimise`/`validate-basket` | Creates an in-memory SQLite database, applies the Phase 0 schema migration only (`001_phase0_schema.sql`), seeds it from fixture files, and runs the normal resolver/optimiser paths. No persistent SQLite file or network is required. The ingest adapter is not invoked. |

### CSRF token pattern

Do not use the stateless double-submit pattern. Use the session-bound synchroniser token: server stores `csrf_token` in the `sessions` row; client reads it from the `csrf` cookie; every POST/PUT/DELETE sends it in `X-CSRF-Token`; server validates against the session record.
