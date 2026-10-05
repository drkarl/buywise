-- ============================================================================
-- Migration 010: sub-categories for shopping-mode grouping (issue #101)
-- ============================================================================
-- Shopping mode grouped items into 11 categories, but some (notably Pantry)
-- span several aisles in a real shop. This migration introduces ONE level of
-- optional sub-category beneath a category so a large Pantry section splits into
-- pasta/rice, tins/jars, sauces, breakfast, baking and spices.
--
-- Design:
--   * `subcategories` is shared reference data, exactly like `categories`: every
--     household sees the same split and order. `default_order` orders the split
--     WITHIN its parent category. Names are bilingual (name_en / name_es) to
--     match `categories`.
--   * `canonical_items.subcategory_id` is nullable. NULL means "not split yet"
--     and shopping mode falls back to the parent `category_id`, so an unmapped
--     item never disappears from a section.
--   * Per-store aisle layouts stay out of scope (issue #101): sub-categories are
--     ordered by `default_order`, not by `store_category_orders`.
--
-- Backfill: the seed mapping below assigns a sub-category to every canonical
-- seed item in a split category, matched on the canonical English name (the
-- stable display name seeded from fixtures/dictionary.json by 002_seed.sql).
-- Items with no mapping keep `subcategory_id` NULL and fall back to the parent
-- category.
-- ============================================================================

PRAGMA foreign_keys = ON;

CREATE TABLE subcategories (
    id            TEXT PRIMARY KEY,
    category_id   TEXT NOT NULL REFERENCES categories(id),
    name_en       TEXT NOT NULL,
    name_es       TEXT NOT NULL,
    default_order INTEGER NOT NULL
);

-- The shopping-mode lookup is "sub-categories of this category, in display
-- order"; the index serves that ordering directly.
CREATE INDEX idx_subcategories_category ON subcategories(category_id, default_order);

-- Shared sub-category catalogue. ids are deterministic ('d1' prefix, mirroring
-- the 'c1' category ids) so the seed is reproducible and references are stable
-- across environments.
--
-- Parent categories (migrations/002_seed.sql):
--   Pantry    = c1000000-0000-0000-0000-000000000005
--   Drinks    = c1000000-0000-0000-0000-000000000008
--   Household = c1000000-0000-0000-0000-000000000010
INSERT INTO subcategories (id, category_id, name_en, name_es, default_order) VALUES
    -- Pantry: the six aisles the captain called out (pasta, tins, sauces,
    -- breakfast, baking, spices).
    ('d1000000-0000-0000-0000-000000000001', 'c1000000-0000-0000-0000-000000000005', 'Pasta & Rice',         'Pasta y Arroz',          1),
    ('d1000000-0000-0000-0000-000000000002', 'c1000000-0000-0000-0000-000000000005', 'Tins & Jars',          'Conservas y Tarros',     2),
    ('d1000000-0000-0000-0000-000000000003', 'c1000000-0000-0000-0000-000000000005', 'Sauces & Condiments',  'Salsas y Condimentos',   3),
    ('d1000000-0000-0000-0000-000000000004', 'c1000000-0000-0000-0000-000000000005', 'Breakfast',            'Desayuno',               4),
    ('d1000000-0000-0000-0000-000000000005', 'c1000000-0000-0000-0000-000000000005', 'Baking',               'Repostería',             5),
    ('d1000000-0000-0000-0000-000000000006', 'c1000000-0000-0000-0000-000000000005', 'Spices',               'Especias',               6),
    -- Drinks.
    ('d1000000-0000-0000-0000-000000000011', 'c1000000-0000-0000-0000-000000000008', 'Water',                'Agua',                   1),
    ('d1000000-0000-0000-0000-000000000012', 'c1000000-0000-0000-0000-000000000008', 'Juice & Smoothies',    'Zumos y Batidos',        2),
    ('d1000000-0000-0000-0000-000000000013', 'c1000000-0000-0000-0000-000000000008', 'Soft Drinks',          'Refrescos',              3),
    ('d1000000-0000-0000-0000-000000000014', 'c1000000-0000-0000-0000-000000000008', 'Beer & Wine',          'Cerveza y Vino',         4),
    -- Household.
    ('d1000000-0000-0000-0000-000000000021', 'c1000000-0000-0000-0000-000000000010', 'Cleaning Supplies',    'Productos de Limpieza',  1),
    ('d1000000-0000-0000-0000-000000000022', 'c1000000-0000-0000-0000-000000000010', 'Laundry',              'Lavandería',             2),
    ('d1000000-0000-0000-0000-000000000023', 'c1000000-0000-0000-0000-000000000010', 'Paper & Kitchen',      'Papel y Cocina',         3),
    ('d1000000-0000-0000-0000-000000000024', 'c1000000-0000-0000-0000-000000000010', 'Household Extras',     'Extras del Hogar',       4);

-- Nullable one-level split. Existing rows keep NULL and fall back to the parent
-- category in shopping mode, so nothing disappears before the backfill runs.
ALTER TABLE canonical_items ADD COLUMN subcategory_id TEXT REFERENCES subcategories(id);

CREATE INDEX idx_canonical_items_subcategory ON canonical_items(subcategory_id);

-- ---- Seed mapping: canonical item name_en -> sub-category -------------------
-- Matched on `name_en`, the stable canonical display name seeded from
-- fixtures/dictionary.json. Any canonical item not listed here (custom items,
-- items added later) keeps subcategory_id = NULL and falls back to its parent
-- category.

-- Pantry → Pasta & Rice
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000001'
WHERE category_id = 'c1000000-0000-0000-0000-000000000005'
  AND name_en IN ('Pasta', 'Spaghetti', 'Macaroni', 'Rice', 'White rice', 'Brown rice');

-- Pantry → Tins & Jars
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000002'
WHERE category_id = 'c1000000-0000-0000-0000-000000000005'
  AND name_en IN ('Canned beans', 'Canned tomatoes', 'Black beans', 'Chicken broth', 'Jam');

-- Pantry → Sauces & Condiments
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000003'
WHERE category_id = 'c1000000-0000-0000-0000-000000000005'
  AND name_en IN ('Ketchup', 'Mayonnaise', 'Mustard', 'Hot sauce', 'Soy sauce',
                  'Tomato sauce', 'Vinegar', 'Olive oil', 'Vegetable oil');

-- Pantry → Breakfast (incl. the peanut-butter spread)
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000004'
WHERE category_id = 'c1000000-0000-0000-0000-000000000005'
  AND name_en IN ('Cereal', 'Oatmeal', 'Coffee', 'Tea', 'Honey', 'Peanut butter');

-- Pantry → Baking
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000005'
WHERE category_id = 'c1000000-0000-0000-0000-000000000005'
  AND name_en IN ('Flour', 'Sugar', 'Brown sugar');

-- Pantry → Spices
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000006'
WHERE category_id = 'c1000000-0000-0000-0000-000000000005'
  AND name_en IN ('Salt', 'Pepper');

-- Drinks → Water
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000011'
WHERE category_id = 'c1000000-0000-0000-0000-000000000008'
  AND name_en IN ('Water', 'Sparkling water', 'Coconut water');

-- Drinks → Juice & Smoothies
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000012'
WHERE category_id = 'c1000000-0000-0000-0000-000000000008'
  AND name_en IN ('Apple juice', 'Orange juice', 'Juice', 'Smoothie');

-- Drinks → Soft Drinks
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000013'
WHERE category_id = 'c1000000-0000-0000-0000-000000000008'
  AND name_en IN ('Cola', 'Soda', 'Lemonade', 'Energy drink', 'Sports drink',
                  'Iced tea', 'Hot chocolate');

-- Drinks → Beer & Wine
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000014'
WHERE category_id = 'c1000000-0000-0000-0000-000000000008'
  AND name_en IN ('Beer', 'Red wine', 'White wine', 'Wine');

-- Household → Cleaning Supplies
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000021'
WHERE category_id = 'c1000000-0000-0000-0000-000000000010'
  AND name_en IN ('All purpose cleaner', 'Bleach', 'Dish soap', 'Glass cleaner',
                  'Sponge', 'Broom', 'Mop');

-- Household → Laundry
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000022'
WHERE category_id = 'c1000000-0000-0000-0000-000000000010'
  AND name_en IN ('Laundry detergent', 'Fabric softener');

-- Household → Paper & Kitchen
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000023'
WHERE category_id = 'c1000000-0000-0000-0000-000000000010'
  AND name_en IN ('Aluminum foil', 'Plastic wrap', 'Paper towels', 'Tissues',
                  'Toilet paper', 'Trash bags', 'Ziplock bags');

-- Household → Household Extras
UPDATE canonical_items SET subcategory_id = 'd1000000-0000-0000-0000-000000000024'
WHERE category_id = 'c1000000-0000-0000-0000-000000000010'
  AND name_en IN ('Batteries', 'Candles', 'Light bulb', 'Matches');
