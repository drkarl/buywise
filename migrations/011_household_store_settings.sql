-- ============================================================================
-- Migration 011: Household store settings (issue #98)
-- ============================================================================
-- Per-household disposition of each retailer during plan generation, plus the
-- "nearby" convenience flag. Sourced from issue #98 (roadmap #114): a household
-- shops Tesco mostly, treats SuperValu as close and Dunnes as rarely visited.
--
-- Three states per (household, retailer):
--   * active     — always eligible for plans (today's behaviour for every store),
--   * occasional — eligible only when the household marks a trip to it today,
--   * off        — never eligible for plans.
-- `nearby` marks a store whose trip is cheap (e.g. SuperValu, around the corner),
-- so a two-store split that includes it clears a lower second-store convenience
-- threshold (see `NEARBY_THRESHOLD_2_CENTS` in `crates/core/src/optimiser.rs`).
--
-- Conventions (match 003_phase1_schema.sql / 005_household_settings.sql):
--   * Primary keys are TEXT; this table has a composite (household_id, retailer).
--   * Timestamps are TEXT (ISO-8601 UTC).
--   * Booleans are INTEGER 0/1.
--
-- Defaults reproduce the pre-#98 behaviour exactly for a household that sets
-- nothing: every retailer is `active` and no store is `nearby`.
--
-- Seeding: households are created only by SQL seed (004_seed_phase1.sql) — there
-- is no runtime create-household path — so this migration back-fills the five
-- retailers per EXISTING household. New households created later must seed their
-- own rows alongside the household (the settings API treats a missing row as the
-- default active/not-nearby, so a household without rows still plans correctly).
-- ============================================================================

PRAGMA foreign_keys = ON;

CREATE TABLE household_store_settings (
    household_id TEXT NOT NULL REFERENCES households(id),
    -- The canonical Retailer DB strings (domain.rs `Retailer::as_db_str`).
    retailer     TEXT NOT NULL
                 CHECK (retailer IN ('tesco', 'dunnes', 'supervalu', 'aldi', 'lidl')),
    status       TEXT NOT NULL DEFAULT 'active'
                 CHECK (status IN ('active', 'occasional', 'off')),
    nearby       INTEGER NOT NULL DEFAULT 0 CHECK (nearby IN (0, 1)),
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (household_id, retailer)
);

-- Back-fill the default (active, not nearby) row for every existing household ×
-- retailer. A fixed ISO-8601 literal keeps the migration deterministic; the
-- CROSS JOIN over the closed retailer vocabulary supplies one row per store.
-- (sqlx's _sqlx_migrations versioning never re-runs an applied migration, so this
-- INSERT executes exactly once per database; INSERT OR IGNORE is belt-and-braces
-- against a household that somehow already carries a row.)
INSERT OR IGNORE INTO household_store_settings
    (household_id, retailer, status, nearby, updated_at)
SELECT h.id, r.retailer, 'active', 0, '2025-01-01T00:00:00Z'
FROM households h
CROSS JOIN (
    SELECT 'tesco' AS retailer
    UNION ALL SELECT 'dunnes'
    UNION ALL SELECT 'supervalu'
    UNION ALL SELECT 'aldi'
    UNION ALL SELECT 'lidl'
) r;
