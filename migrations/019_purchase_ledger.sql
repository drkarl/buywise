-- ============================================================================
-- Migration 019: Purchase ledger — what the household actually bought
-- (issue #148, roadmap #114; the core model for receipts #116 / #117 / #118).
-- ============================================================================
-- `purchase_events` (migration 013) records only the *check-off timing* of a
-- canonical item: it has no quantity, product, size or amount, so it is not a
-- money record. This migration adds the household's purchase ledgers — one row
-- per bought transaction and one per line — so a retailer export (today) or a
-- receipt read (later) can feed a history that carries quantities and amounts.
--
-- Three additive tables (no rebuild, no change to any existing table):
--
--   * `purchase_transactions` — one row per purchase the household made at one
--     retailer on one date, with its total, currency, source identity and the
--     original source payload (JSON TEXT, stored verbatim).
--   * `purchase_lines` — one row per line of a transaction, in `line_ordinal`
--     order, carrying the original text plus whatever the source knew: name,
--     brand, quantity, size, unit price, line total, discount, deposit, and
--     optional links to a retailer product and a canonical item. A line whose
--     product is unknown stays as `match_status = 'unmatched'` and is never
--     dropped or guessed.
--   * `purchase_transaction_revisions` — when an import corrects an existing
--     transaction its previous revision is snapshotted here (fields plus its
--     lines as JSON) before the live row is updated, so an audit reader keeps
--     the earlier version. The live row's `revision` is bumped.
--
-- Identity and idempotency (the rule the import format documents):
--
--   * `transaction_identity` is the caller's `source_transaction_id` when one
--     was given, otherwise the computed stable `fingerprint` of
--     retailer + purchase date + total + the ordered line texts. It is NOT NULL,
--     and `UNIQUE(household_id, source_kind, transaction_identity)` is the
--     idempotency key: re-importing the same identity upserts the one row
--     instead of creating a duplicate. An expression index over
--     `COALESCE(source_transaction_id, fingerprint)` is deliberately avoided:
--     SQLite does not treat NULLs as equal in a unique index, so a plain
--     coalesced NOT NULL column is the only form that also gives UPSERT a
--     usable conflict target.
--   * `identity_kind` records which of the two produced the key, so a reader
--     can tell a caller-supplied id from a computed fingerprint without
--     guessing from the NULL.
--   * `UNIQUE(transaction_id, line_ordinal)` keeps each line of a transaction
--     at exactly one ordinal, so replacing a revision's lines cannot leave a
--     stale ordinal behind.
--
-- Money is integer cents, never a float, with an ISO-4217 `currency` code
-- (default `EUR`). `quantity` and `size_value` are REAL because a purchase line
-- can be a decimal weight/volume; every money column is INTEGER.
--
-- Conventions (match 003_phase1_schema.sql / 013_purchase_suggestions.sql):
--   * Primary keys are TEXT (UUID strings).
--   * Timestamps are TEXT (ISO-8601 UTC); `purchase_date` is a calendar date
--     (`YYYY-MM-DD`) in the shop's own timezone and is part of the identity.
--   * Enum-like columns carry an explicit CHECK(... IN (...)).
--   * This table holds the household's own data: it is included in the
--     household export and removed by the household delete
--     (`crates/core/src/household_data.rs`).
-- ============================================================================

PRAGMA foreign_keys = ON;

-- One row per purchase transaction the household imported. `source_kind` says
-- where it came from ('import' today; 'receipt' reserved for receipt reading).
-- `source_transaction_id` is the retailer's own id when the source has one; the
-- computed `fingerprint` is always stored so the identity rule is auditable even
-- when an explicit id was supplied.
CREATE TABLE purchase_transactions (
    id                    TEXT PRIMARY KEY,
    household_id          TEXT NOT NULL REFERENCES households(id),
    retailer              TEXT NOT NULL,
    purchase_date         TEXT NOT NULL,
    source_kind           TEXT NOT NULL DEFAULT 'import'
        CHECK(source_kind IN ('import', 'receipt')),
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
    updated_at            TEXT NOT NULL
);

-- The household-scoped idempotency key: one transaction per (source kind,
-- identity) per household. Re-importing the same identity upserts this row.
CREATE UNIQUE INDEX idx_purchase_transactions_identity
    ON purchase_transactions(household_id, source_kind, transaction_identity);

-- Per-household history lookups by date.
CREATE INDEX idx_purchase_transactions_household_date
    ON purchase_transactions(household_id, purchase_date);

-- One row per line of a transaction. All fields except the original text, the
-- line total and the match status are nullable: a source that does not know the
-- brand, size or unit price leaves them NULL rather than guessing. Deleting the
-- transaction removes its lines.
CREATE TABLE purchase_lines (
    id                   TEXT PRIMARY KEY,
    transaction_id       TEXT NOT NULL
        REFERENCES purchase_transactions(id) ON DELETE CASCADE,
    line_ordinal         INTEGER NOT NULL,
    original_text        TEXT NOT NULL,
    name                 TEXT,
    brand                TEXT,
    quantity             REAL,
    size_value           REAL,
    size_unit            TEXT,
    unit_price_cents     INTEGER,
    line_total_cents     INTEGER NOT NULL,
    line_discount_cents  INTEGER NOT NULL DEFAULT 0,
    deposit_cents        INTEGER NOT NULL DEFAULT 0,
    retailer_product_id  TEXT REFERENCES retailer_products(id),
    canonical_item_id    TEXT REFERENCES canonical_items(id),
    match_status         TEXT NOT NULL DEFAULT 'unmatched'
        CHECK(match_status IN ('unmatched', 'matched')),
    revision             INTEGER NOT NULL DEFAULT 1,
    created_at           TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_purchase_lines_transaction_ordinal
    ON purchase_lines(transaction_id, line_ordinal);

-- Audit trail of superseded revisions. A correction to an imported transaction
-- snapshots the previous revision (its header fields, its source payload and its
-- lines as JSON) here before the live row is updated, so the earlier version is
-- never overwritten silently. Deleting the transaction removes its history.
CREATE TABLE purchase_transaction_revisions (
    id               TEXT PRIMARY KEY,
    transaction_id   TEXT NOT NULL
        REFERENCES purchase_transactions(id) ON DELETE CASCADE,
    revision         INTEGER NOT NULL,
    retailer         TEXT NOT NULL,
    purchase_date    TEXT NOT NULL,
    currency         TEXT NOT NULL,
    total_cents      INTEGER NOT NULL,
    source_payload   TEXT NOT NULL,
    lines_json       TEXT NOT NULL,
    superseded_at    TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_purchase_transaction_revisions_revision
    ON purchase_transaction_revisions(transaction_id, revision);
