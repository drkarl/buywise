-- ============================================================================
-- Migration 004: Phase 1 seed data (default household + store aisle orders)
-- ============================================================================
-- Source of truth: docs/implementation-plan.md (P1-T1).
-- Seeds the canonical default household and a baseline aisle ordering for every
-- (store, category) pair so generated routes have a sensible default walk.
--
-- CRITICAL: the household id MUST equal the household used by the Phase 0
-- canonical items / fixtures (00000000-0000-0000-0000-000000000001, see
-- 002_seed.sql / fixtures/canonical_items.json). If Phase 1 seeded a different
-- household, /api/items would return nothing for the seeded catalogue.
--
-- Household users are NOT seeded here: their argon2id password_hash is supplied
-- at deploy time (env vars / seed script, see P1-T2), never committed.
-- ============================================================================

PRAGMA foreign_keys = ON;

-- The canonical default household. INSERT OR IGNORE so re-applying on a DB that
-- already has the row (e.g. created by a future bootstrap) is a no-op.
INSERT OR IGNORE INTO households (id, name, created_at) VALUES
    ('00000000-0000-0000-0000-000000000001', 'Home', '2025-01-01T00:00:00Z');

-- Default store_category_orders: one row per (store, category).
-- Expects exactly 11 categories from 002_seed.sql -> 5 stores x 11 = 55 rows.
-- crates/core/tests/migrations.rs asserts BOTH categories = 11 AND
-- store_category_orders = 55, so any drift in the category seed fails the test.
--
-- We INSERT ... SELECT against the live `categories` table rather than hard-code
-- category ids: the category primary keys are generated UUIDs (002_seed.sql), so
-- cross-joining the store list against the seeded categories references the EXACT
-- ids that exist and always yields exactly 5 x 11 = 55 rows.
--
-- The id is derived deterministically from the stable (store, category_id) pair
-- as 'sco-' || store || '-' || category_id (pure SQLite, no randomness),
-- mirroring 002_seed.sql's reproducible-id discipline so re-running yields the
-- same rows. display_order seeds from the category default_order; households can
-- override per-store later. Exact per-store aisle ordering is an editorial call
-- (the design plan mentions e.g. Tesco PRODUCE -> BAKERY -> DAIRY -> ...); we
-- seed a uniform default_order baseline here and leave per-store curation to a
-- later editorial pass.
INSERT INTO store_category_orders (id, household_id, store, category_id, display_order, updated_at)
SELECT
    'sco-' || s.store || '-' || c.id           AS id,
    '00000000-0000-0000-0000-000000000001'     AS household_id,
    s.store,
    c.id                                       AS category_id,
    c.default_order                            AS display_order,
    '2025-01-01T00:00:00Z'                     AS updated_at
FROM categories c
CROSS JOIN (
    SELECT 'tesco'     AS store
    UNION ALL SELECT 'dunnes'
    UNION ALL SELECT 'supervalu'
    UNION ALL SELECT 'aldi'
    UNION ALL SELECT 'lidl'
) s;
