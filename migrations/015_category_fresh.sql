-- ============================================================================
-- Migration 015: mark fresh-food categories (issue #100)
-- ============================================================================
-- The Aldi comparison (issue #100) splits what Aldi would have cost the basket
-- into packaged and fresh items, because the household's quality concern about
-- Aldi is about fresh food (produce, meat/fish, dairy, bakery). "Fresh" is a
-- property of the shared category catalogue, so it lives next to the category
-- rather than being guessed from names or ids in code.
--
-- Design:
--   * `is_fresh` is 0/1. It is independent of the household's per-store category
--     rules (migration 014): the comparison must be computed even when Aldi is
--     switched off or its fresh categories are excluded, so the flag describes
--     the item, not any store policy.
--   * Default 0 ("packaged") for every category, including ones created later by
--     a household; the four fresh categories seeded by 002_seed.sql are marked
--     below. An item with no category is therefore packaged (see
--     docs/KNOWN_LIMITATIONS.md, "Aldi comparison").
--   * Reference data is shared, like the rest of `categories`: the flag is not
--     household-scoped.
-- ============================================================================

PRAGMA foreign_keys = ON;

ALTER TABLE categories ADD COLUMN is_fresh INTEGER NOT NULL DEFAULT 0
    CHECK (is_fresh IN (0, 1));

-- The four fresh-food categories in the shared catalogue (002_seed.sql):
--   Produce       c1000000-0000-0000-0000-000000000001
--   Meat & Fish   c1000000-0000-0000-0000-000000000002
--   Dairy         c1000000-0000-0000-0000-000000000003
--   Bakery        c1000000-0000-0000-0000-000000000004
UPDATE categories SET is_fresh = 1 WHERE id IN (
    'c1000000-0000-0000-0000-000000000001',
    'c1000000-0000-0000-0000-000000000002',
    'c1000000-0000-0000-0000-000000000003',
    'c1000000-0000-0000-0000-000000000004'
);
