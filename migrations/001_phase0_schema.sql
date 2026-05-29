-- 001_phase0_schema.sql
--
-- Phase 0 schema for BuyWise (see docs/design.md §7 and docs/implementation-plan.md
-- "P0-T2: SQLite migrations — Phase 0 tables").
--
-- Nine tables: categories, canonical_items, item_aliases, item_constraints,
-- source_runs, retailer_products, product_offers, product_matches, api_usage_events.
--
-- Design notes:
--   * product_offers is append-only; `source_successful_run_number_at_fetch` is
--     denormalised at ingest and never updated (no trigger by design).
--   * Money is stored as integer cents / micros — never floats.
--   * Partial unique indexes guard API-sourced uniqueness while allowing manual
--     rows without external IDs and failed runs without run numbers.

CREATE TABLE categories (
    id          TEXT PRIMARY KEY,
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
    created_at              TEXT NOT NULL
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
    allowed_brands          TEXT,
    excluded_brands         TEXT,
    preferred_size_value    REAL,
    preferred_size_basis    TEXT CHECK(preferred_size_basis IN ('g','ml','each') OR preferred_size_basis IS NULL),
    min_size_value          REAL,
    min_size_basis          TEXT CHECK(min_size_basis IN ('g','ml','each') OR min_size_basis IS NULL),
    max_size_value          REAL,
    max_size_basis          TEXT CHECK(max_size_basis IN ('g','ml','each') OR max_size_basis IS NULL),
    pack_count              INTEGER,
    form_factor             TEXT,
    dietary_flags           TEXT,
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
    successful_run_number INTEGER
);
CREATE UNIQUE INDEX uq_source_runs_source_run_number
    ON source_runs(source, successful_run_number)
    WHERE successful_run_number IS NOT NULL;

CREATE TABLE retailer_products (
    id              TEXT PRIMARY KEY,
    source          TEXT NOT NULL CHECK(source IN ('basketwatch','lidl_direct','manual')),
    retailer        TEXT NOT NULL CHECK(retailer IN ('tesco','dunnes','supervalu','aldi','lidl')),
    external_id     TEXT,
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
CREATE UNIQUE INDEX uq_retailer_products_source_external
    ON retailer_products(source, retailer, external_id)
    WHERE source != 'manual' AND external_id IS NOT NULL;

CREATE TABLE product_offers (
    id                                    TEXT PRIMARY KEY,
    retailer_product_id                   TEXT NOT NULL REFERENCES retailer_products(id),
    price_cents                           INTEGER NOT NULL,
    unit_price_eur_micros                 INTEGER,
    unit_price_basis                      TEXT,
    previous_price_cents                  INTEGER,
    loyalty_price_cents                   INTEGER,
    loyalty_scheme                        TEXT CHECK(loyalty_scheme IN ('clubcard','real_rewards') OR loyalty_scheme IS NULL),
    offer_text                            TEXT,
    offer_valid_until                     TEXT,
    source_snapshot_date                  TEXT,
    fetched_at                            TEXT NOT NULL,
    cache_expires_at                      TEXT,
    last_successful_source_run_id         TEXT REFERENCES source_runs(id),
    source_successful_run_number_at_fetch INTEGER,
    source_confidence_at_ingest           TEXT NOT NULL
        CHECK(source_confidence_at_ingest IN ('high','medium','low'))
);
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

CREATE TABLE api_usage_events (
    id             TEXT PRIMARY KEY,
    source         TEXT NOT NULL,
    endpoint       TEXT NOT NULL,
    requests_count INTEGER NOT NULL,
    month_year     TEXT NOT NULL,
    created_at     TEXT NOT NULL
);
