-- ============================================================================
-- Migration 014: per-store category rules (issue #99)
-- ============================================================================
-- A household shops at a store only for the categories it trusts it for: Aldi's
-- fresh food is considered lower quality, but its packaged food is acceptable.
-- This migration adds the rules the optimiser consults before assigning an item
-- to a retailer.
--
-- Design:
--   * One row per (household, retailer, scope) where scope is a category
--     (`subcategory_id IS NULL`) or, as a refinement, one of that category's
--     sub-categories (`subcategory_id` set — migration 010).
--   * `allowed` is 0 or 1. Matching is most-specific-first: a rule for the
--     item's sub-category beats a rule for its category; with no matching rule
--     the item is allowed. So a store with no rows at all allows every category,
--     reproducing the pre-#99 behaviour exactly (mirroring migration 011's
--     default `active` store settings).
--   * Reference data is shared: rules reference the existing `categories` and
--     `subcategories` catalogues — no new taxonomy or abstraction.
--   * Retailer strings are the canonical Retailer DB strings (domain.rs
--     `Retailer::as_db_str`), the same vocabulary migration 011 constrains.
--
-- The unique index uses `COALESCE(subcategory_id, '')` because a SQLite PRIMARY
-- KEY / UNIQUE constraint treats NULLs as distinct, and this table needs one row
-- per (household, retailer, category, whole-category scope) too. The API writes
-- whole sets (delete-then-insert) per store, so the index is a guard rather than
-- the primary mechanism.
-- ============================================================================

PRAGMA foreign_keys = ON;

CREATE TABLE household_store_category_rules (
    household_id   TEXT NOT NULL REFERENCES households(id),
    retailer       TEXT NOT NULL
                   CHECK (retailer IN ('tesco', 'dunnes', 'supervalu', 'aldi', 'lidl')),
    category_id    TEXT NOT NULL REFERENCES categories(id),
    -- NULL means the rule covers the whole category.
    subcategory_id TEXT REFERENCES subcategories(id),
    allowed        INTEGER NOT NULL CHECK (allowed IN (0, 1)),
    updated_at     TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_household_store_category_rules_unique
    ON household_store_category_rules
       (household_id, retailer, category_id, COALESCE(subcategory_id, ''));

-- The optimiser reads a household's whole rule set for one store at a time, and
-- the settings endpoint reads every store's rules at once; this index serves the
-- household+retailer lookup directly.
CREATE INDEX idx_household_store_category_rules_store
    ON household_store_category_rules (household_id, retailer);
