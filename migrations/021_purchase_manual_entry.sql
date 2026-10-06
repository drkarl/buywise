-- ============================================================================
-- Migration 021: Manual purchase entry and the receipt-photo link
-- (issue #156, roadmap #114)
-- ============================================================================
-- The purchase ledger (migration 019) could only be filled by an operator
-- import. This migration makes the household's own hand-entered purchases
-- possible and lets a purchase point at the photo of the receipt it records.
--
-- Two schema changes, both on `purchase_transactions`:
--
--   * `receipt_image_id` — a nullable reference to `receipt_images(id)` with
--     `ON DELETE SET NULL`. A purchase may be linked to one of the household's
--     receipt photos; when that image is deleted or expires (the daily
--     `expire-receipts` sweep, or the single-image/household delete) the link
--     clears itself and the purchase and its lines stay. `NULL` means no link.
--   * `source_kind` gains the value `'manual'` for a purchase a member typed by
--     hand (the import path keeps `'import'`; `'receipt'` stays reserved for
--     receipt reading).
--
-- Why this migration REBUILDS the table
-- -------------------------------------
-- SQLite cannot add or alter a CHECK constraint with `ALTER TABLE` (see
-- migrations 008, 009, 012, 013, which rebuild for the same reason). Adding
-- `'manual'` to the `source_kind` CHECK therefore needs a table rebuild — the
-- plain `ADD COLUMN` the ledger's photo link alone would allow cannot widen it.
-- Both changes are made in one rebuild.
--
-- The rebuild is safe for the two child tables:
--
--   * `purchase_lines` and `purchase_transaction_revisions` reference
--     `purchase_transactions(id)` `ON DELETE CASCADE`, and sqlx runs each
--     migration inside a transaction (so `PRAGMA foreign_keys` cannot be turned
--     off here, and cannot be relied on to protect the children from the
--     `DROP TABLE`).
--   * Their rows are therefore copied into temporary tables, the (now empty)
--     child tables are cleared, the parent is rebuilt, the parent's indexes are
--     recreated, and the child rows are restored. No row is dropped, and the
--     `ON DELETE CASCADE` behaviour is preserved for the future.
--
-- Conventions (match 019_purchase_ledger.sql):
--   * Additive to the household's data: one nullable column plus one widened
--     CHECK; every existing row and column value is copied across unchanged.
--   * Money is integer cents; timestamps are TEXT (ISO-8601 UTC).
-- ============================================================================

PRAGMA foreign_keys = ON;

-- Stage the child rows. `CREATE TABLE ... AS SELECT` copies both the schema
-- shape and the rows; the child tables themselves are untouched, so their
-- foreign keys and indexes survive the parent rebuild and only need refilling.
CREATE TEMP TABLE _m021_purchase_lines_backup AS SELECT * FROM purchase_lines;
CREATE TEMP TABLE _m021_purchase_revisions_backup AS
    SELECT * FROM purchase_transaction_revisions;

-- Clear the children first so dropping the old parent fires no ON DELETE
-- CASCADE on real rows (the cascade would otherwise delete them all).
DELETE FROM purchase_lines;
DELETE FROM purchase_transaction_revisions;

-- The rebuilt parent: the widened `source_kind` set plus `receipt_image_id`.
CREATE TABLE purchase_transactions_new (
    id                    TEXT PRIMARY KEY,
    household_id          TEXT NOT NULL REFERENCES households(id),
    retailer              TEXT NOT NULL,
    purchase_date         TEXT NOT NULL,
    source_kind           TEXT NOT NULL DEFAULT 'import'
        CHECK(source_kind IN ('import', 'receipt', 'manual')),
    source_transaction_id TEXT,
    transaction_identity  TEXT NOT NULL,
    identity_kind         TEXT NOT NULL
        CHECK(identity_kind IN ('source_id', 'fingerprint')),
    fingerprint           TEXT NOT NULL,
    currency              TEXT NOT NULL DEFAULT 'EUR',
    total_cents           INTEGER NOT NULL,
    revision              INTEGER NOT NULL DEFAULT 1,
    source_payload        TEXT NOT NULL,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    -- The receipt photo this purchase documents, if any. Nullable and
    -- ON DELETE SET NULL: an image that is deleted or expires clears the link
    -- while the purchase and its lines stay. SQLite permits this REFERENCES
    -- clause on a new column because its default is NULL.
    receipt_image_id      TEXT REFERENCES receipt_images(id) ON DELETE SET NULL
);

INSERT INTO purchase_transactions_new
    (id, household_id, retailer, purchase_date, source_kind, source_transaction_id,
     transaction_identity, identity_kind, fingerprint, currency, total_cents, revision,
     source_payload, created_at, updated_at, receipt_image_id)
    SELECT id, household_id, retailer, purchase_date, source_kind, source_transaction_id,
           transaction_identity, identity_kind, fingerprint, currency, total_cents, revision,
           source_payload, created_at, updated_at, NULL
    FROM purchase_transactions;

DROP TABLE purchase_transactions;
ALTER TABLE purchase_transactions_new RENAME TO purchase_transactions;

-- The old table's indexes were dropped with it; recreate them verbatim from 019.
CREATE UNIQUE INDEX idx_purchase_transactions_identity
    ON purchase_transactions(household_id, retailer, source_kind, transaction_identity);
CREATE INDEX idx_purchase_transactions_household_date
    ON purchase_transactions(household_id, purchase_date);

-- Restore the child rows against the rebuilt parent, then drop the staging.
INSERT INTO purchase_lines SELECT * FROM _m021_purchase_lines_backup;
INSERT INTO purchase_transaction_revisions SELECT * FROM _m021_purchase_revisions_backup;
DROP TABLE _m021_purchase_lines_backup;
DROP TABLE _m021_purchase_revisions_backup;
