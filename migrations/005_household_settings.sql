-- ============================================================================
-- Migration 005: Household settings (configurable convenience thresholds)
-- ============================================================================
-- Source of truth: docs/implementation-plan.md (P4-T2). Adds a per-household
-- settings row holding the two convenience thresholds (cents) that the basket
-- optimiser consults when a `POST /api/optimise` request omits an explicit
-- threshold. Until now those defaults were core constants (€10 / €25); this
-- table lets a household override them via `GET`/`PATCH /api/settings`.
--
-- Conventions (match 003_phase1_schema.sql):
--   * Primary keys are TEXT.
--   * Timestamps are TEXT (ISO-8601 UTC).
--   * Money is INTEGER cents — never floats.
--
-- Defaults mirror the core constants so a freshly back-filled row reproduces the
-- pre-P4-T2 behaviour exactly:
--   * convenience_threshold_2_cents = 1000  (€10.00, DEFAULT_THRESHOLD_2_CENTS)
--   * convenience_threshold_3_cents = 2500  (€25.00, DEFAULT_THRESHOLD_3_CENTS)
--
-- Seeding: households are created only by SQL seed (004_seed_phase1.sql) — there
-- is no runtime create-household path — so this migration back-fills one default
-- row for every EXISTING household. New households created later (by a future
-- bootstrap path) must seed their own settings row alongside the household.
-- ============================================================================

PRAGMA foreign_keys = ON;

CREATE TABLE household_settings (
    household_id                  TEXT PRIMARY KEY REFERENCES households(id),
    convenience_threshold_2_cents INTEGER NOT NULL DEFAULT 1000,   -- €10.00
    convenience_threshold_3_cents INTEGER NOT NULL DEFAULT 2500,   -- €25.00
    updated_at                    TEXT NOT NULL,
    -- A third-store threshold can never be easier to justify than a second, so a
    -- stored pair must be ordered. Enforced at the schema level (not just in the
    -- API handlers) so an out-of-band write cannot persist an invalid pair that
    -- would make /api/optimise and /api/settings 422 for that household until
    -- repaired. Mirrors the 422 guard in settings.rs / optimise.rs.
    CHECK (convenience_threshold_3_cents >= convenience_threshold_2_cents)
);

-- Back-fill a default settings row for every existing household. A fixed
-- ISO-8601 literal is used (migrations must be deterministic, not depend on
-- wall-clock time); the SELECT supplies the documented €10 / €25 defaults.
-- (sqlx's _sqlx_migrations versioning never re-runs an applied migration, so
-- this INSERT executes exactly once per database; INSERT OR IGNORE is belt-and-
-- braces against a household that somehow already carries a settings row.)
INSERT OR IGNORE INTO household_settings
    (household_id, convenience_threshold_2_cents, convenience_threshold_3_cents, updated_at)
SELECT id, 1000, 2500, '2025-01-01T00:00:00Z'
FROM households;
