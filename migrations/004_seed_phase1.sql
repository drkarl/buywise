-- ============================================================================
-- Migration 004: Phase 1 seed data (default household + store aisle orders)
-- ============================================================================
-- Source of truth: docs/design.md §7, docs/implementation-plan.md (P1-T1).
-- Seeds the canonical default household and a baseline aisle ordering for every
-- (retailer, category) pair so generated routes have a sensible default walk.
-- The household id MUST match the household used by the Phase 0 canonical
-- items / fixtures: 00000000-0000-0000-0000-000000000001.
-- Timestamps are TEXT (ISO-8601 UTC). Ids are stable TEXT keys.
-- ============================================================================

PRAGMA foreign_keys = ON;

-- The canonical default household. MUST equal the Phase 0 canonical-items
-- household id (see migrations/002_seed.sql / fixtures/canonical_items.json) so
-- cross-phase data lines up.
INSERT INTO households (id, name, created_at) VALUES
    ('00000000-0000-0000-0000-000000000001', 'Default Household', '2025-01-01T00:00:00Z');

-- Default store_category_orders: one row per (retailer, category).
-- 5 retailers x 11 categories (seeded in 002_seed.sql) = 55 rows.
--
-- We INSERT ... SELECT against the live `categories` table rather than hard-code
-- category ids: the category primary keys are generated UUIDs (see 002_seed.sql),
-- so cross-joining the retailer list against the seeded categories references
-- the EXACT ids that exist and always yields exactly 5 x 11 = 55 rows. The
-- composite id keeps each row stable and unique; the category `default_order`
-- seeds the per-retailer `sort_order` (households can override later).
INSERT INTO store_category_orders (id, household_id, retailer, category_id, sort_order, created_at)
SELECT
    'sco-' || r.retailer || '-' || c.id            AS id,
    '00000000-0000-0000-0000-000000000001'         AS household_id,
    r.retailer,
    c.id                                           AS category_id,
    c.default_order                                AS sort_order,
    '2025-01-01T00:00:00Z'                         AS created_at
FROM categories c
CROSS JOIN (
    SELECT 'tesco'     AS retailer
    UNION ALL SELECT 'dunnes'
    UNION ALL SELECT 'supervalu'
    UNION ALL SELECT 'lidl'
    UNION ALL SELECT 'aldi'
) r;
