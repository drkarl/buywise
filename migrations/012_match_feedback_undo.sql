-- ============================================================================
-- Migration 012: Add `undo` to the match_feedback.action CHECK
-- ============================================================================
-- Issue #108 lets a household correct a mistaken product match from the app.
-- "Undo" resets a match to `pending` (no decision) so the product is proposed
-- again as a candidate, and appends an `undo` row to `match_feedback` so the
-- correction is part of the same append-only decision trail as accept/reject/
-- prefer — the accepted and rejected feedback itself is preserved, never deleted.
--
-- SQLite cannot add/alter a column CHECK via ALTER TABLE, so the constraint is
-- changed the supported way (matching migrations 008/009): build a new table with
-- the widened CHECK, copy the rows, drop the old table, and rename. The table is
-- append-only history, so the copy is a straight `INSERT ... SELECT *` with no
-- transformation.
--
-- Column order/types/constraints below are copied verbatim from
-- 003_phase1_schema.sql except for the one widened CHECK.
-- ============================================================================

PRAGMA foreign_keys = ON;

-- 1. New table with the widened action CHECK (everything else verbatim).
CREATE TABLE match_feedback_new (
    id                TEXT PRIMARY KEY,
    product_match_id  TEXT NOT NULL REFERENCES product_matches(id),
    user_id           TEXT NOT NULL REFERENCES users(id),
    action            TEXT NOT NULL CHECK(action IN ('accept', 'reject', 'prefer', 'undo')),
    created_at        TEXT NOT NULL
);

-- 2. Copy existing rows verbatim (append-only history; no transformation).
INSERT INTO match_feedback_new
    SELECT id, product_match_id, user_id, action, created_at
    FROM match_feedback;

-- 3. Swap the tables.
DROP TABLE match_feedback;
ALTER TABLE match_feedback_new RENAME TO match_feedback;

-- 4. Recreate the index dropped with the old table (verbatim from 003).
CREATE INDEX idx_match_feedback_product_match ON match_feedback(product_match_id);
