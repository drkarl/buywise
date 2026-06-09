-- ============================================================================
-- Migration 007: retailer_products form_factor + dietary_flags (Backlog-T1)
-- ============================================================================
-- Source of truth: docs/implementation-plan.md (Backlog-T1) and the enums in
-- crates/core/src/domain.rs. Adds the two product-side columns the optimiser's
-- full constraint enforcement compares against:
--
--   * form_factor   TEXT  — the physical product form (a single lowercase token
--                           from domain::FORM_FACTORS: 'can', 'bottle',
--                           'carton', 'pouch', 'jar', 'tub', 'bar', 'spray',
--                           'box', 'bag', 'tin', 'tetra'). Compared
--                           case-insensitively against item_constraints.form_factor.
--   * dietary_flags TEXT  — a JSON array of lowercase snake_case tokens from
--                           domain::DIETARY_FLAGS (e.g. ["gluten_free","vegan"]).
--                           A product qualifies only if its set is a SUPERSET of
--                           the required item_constraints.dietary_flags set.
--
-- item_constraints already carries form_factor (TEXT) and dietary_flags (JSON
-- TEXT) since 003_phase1_schema.sql; retailer_products had no matching columns,
-- so those constraints could not be enforced. This migration closes that gap.
--
-- Conventions (match 001_phase0_schema.sql / 003_phase1_schema.sql):
--   * Timestamps are TEXT (ISO-8601 UTC).
--   * JSON-array attributes are stored as TEXT (no sqlx json feature in-tree;
--     decoded in domain::RetailerProduct's hand-written FromRow), mirroring how
--     item_constraints stores allowed_brands / excluded_brands / dietary_flags.
--
-- Both columns are nullable with no default: existing rows (and any product
-- whose source text does not reveal a form/dietary attribute) carry NULL, which
-- the optimiser treats as "unknown" — an absent form/flag set satisfies only an
-- absent constraint. Ingest (basketwatch + lidl adapters) now populates them
-- best-effort from product name + category text.
--
-- ALTER TABLE ADD COLUMN is safe and non-recreating here: neither column is part
-- of a CHECK or the partial unique index, so SQLite's add-column limitations do
-- not apply.
-- ============================================================================

PRAGMA foreign_keys = ON;

ALTER TABLE retailer_products ADD COLUMN form_factor TEXT;
ALTER TABLE retailer_products ADD COLUMN dietary_flags TEXT;
