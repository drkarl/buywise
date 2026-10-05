-- ============================================================================
-- Migration 013: Purchase-history evidence, suggestion dismissals, and the
-- `dismiss_suggestion` mutation operation.
-- ============================================================================
-- Source of truth: issue #106 (purchase suggestions, part of roadmap #114).
-- BuyWise suggests what the household is likely to need next by estimating, per
-- canonical item, the typical interval between check-offs. Two tables record the
-- durable evidence and the user's explicit "not now" control, and the mutations
-- log gains one operation so a dismissal syncs offline like every other edit:
--
--   add_item | mark_bought | mark_skipped | move_to_list | delete_item
--   | rename_item | resolve_alias | reject_match | set_constraint | set_quantity
--   | dismiss_suggestion                                              <-- new
--
-- design note (why evidence is a separate table, not derived from list_entries):
--   A check-off lives on a `list_entries` row whose status can later change
--   (move_to_list undoes it) or whose row can be deleted. The purchase evidence
--   must outlive the row so history keeps building, and it must be able to carry
--   a LATER source (a receipt import) without double counting the same purchase.
--   `source` is therefore part of the row now; version one only writes
--   'check_off'. The suggestion computation collapses same-day rows, so a future
--   receipt and the check-off that confirmed the same shop fold together.
--
-- Conventions (match 003_phase1_schema.sql):
--   * Primary keys are TEXT (UUID or content-hash strings).
--   * Timestamps are TEXT (ISO-8601 UTC).
--   * Enum-like columns carry an explicit CHECK(... IN (...)).
-- ============================================================================

PRAGMA foreign_keys = ON;

-- One row per observed purchase of a canonical item by a household. Append-only:
-- `recorded_at` is the write time, `occurred_at` is when the purchase happened
-- (the check-off's `bought_at`, or a receipt's own timestamp later).
-- `entry_id`/`list_id` are nullable so a future receipt/import source can record
-- a purchase with no list entry behind it. A check-off writes both, and
-- `move_to_list` deletes the check_off row so an accidental check-off leaves no
-- evidence.
CREATE TABLE purchase_events (
    id                TEXT PRIMARY KEY,
    household_id      TEXT NOT NULL REFERENCES households(id),
    canonical_item_id TEXT NOT NULL REFERENCES canonical_items(id),
    list_id           TEXT REFERENCES shopping_lists(id),
    entry_id          TEXT REFERENCES list_entries(id),
    source            TEXT NOT NULL DEFAULT 'check_off'
        CHECK(source IN ('check_off', 'receipt', 'import')),
    occurred_at       TEXT NOT NULL,
    recorded_at       TEXT NOT NULL
);

-- Per-household history lookups (the suggestion computation scans one household).
CREATE INDEX idx_purchase_events_household_item
    ON purchase_events(household_id, canonical_item_id, occurred_at);

-- Append-only audit of the user dismissing a suggestion. A dismissal suppresses
-- the next suggestion until one typical interval has elapsed from
-- `dismissed_at`, so the item resurfaces later rather than never.
CREATE TABLE purchase_dismissals (
    id                TEXT PRIMARY KEY,
    household_id      TEXT NOT NULL REFERENCES households(id),
    canonical_item_id TEXT NOT NULL REFERENCES canonical_items(id),
    dismissed_by      TEXT NOT NULL REFERENCES users(id),
    dismissed_at      TEXT NOT NULL
);

CREATE INDEX idx_purchase_dismissals_household_item
    ON purchase_dismissals(household_id, canonical_item_id, dismissed_at);

-- ---------------------------------------------------------------------------
-- Widen the mutations.operation_type CHECK with `dismiss_suggestion`.
-- ---------------------------------------------------------------------------
-- SQLite cannot add/alter a column CHECK via ALTER TABLE, so the constraint is
-- changed the only supported way: build a new table with the widened CHECK, copy
-- the rows, drop the old table, and rename. The `mutations` table is append-only
-- (idempotency log), so the copy is a straight `INSERT ... SELECT` with no
-- transformation. Column order/types/constraints below are copied verbatim from
-- 009_mutations_add_move_to_list.sql except for the one widened CHECK.
CREATE TABLE mutations_new (
    id                  TEXT PRIMARY KEY,        -- server-assigned UUID
    client_mutation_id  TEXT NOT NULL UNIQUE,    -- client UUID; idempotency key
    device_id           TEXT NOT NULL,
    user_id             TEXT NOT NULL REFERENCES users(id),
    list_id             TEXT NOT NULL REFERENCES shopping_lists(id),
    entity_type         TEXT NOT NULL,
    entity_id           TEXT NOT NULL,
    operation_type      TEXT NOT NULL
        CHECK(operation_type IN ('add_item', 'mark_bought', 'mark_skipped',
                                 'delete_item', 'rename_item', 'resolve_alias',
                                 'reject_match', 'set_constraint', 'set_quantity',
                                 'move_to_list', 'dismiss_suggestion')),
    payload             TEXT NOT NULL,           -- JSON
    base_revision       INTEGER NOT NULL,
    created_at_client   TEXT NOT NULL,
    received_at_server  TEXT NOT NULL,
    server_revision     INTEGER,                 -- NULL until accepted
    status              TEXT NOT NULL DEFAULT 'accepted'
        CHECK(status IN ('accepted', 'rejected', 'merged')),
    response_json       TEXT                     -- stored response for idempotent replay
);

INSERT INTO mutations_new
    SELECT id, client_mutation_id, device_id, user_id, list_id, entity_type,
           entity_id, operation_type, payload, base_revision, created_at_client,
           received_at_server, server_revision, status, response_json
    FROM mutations;

DROP TABLE mutations;
ALTER TABLE mutations_new RENAME TO mutations;

CREATE INDEX idx_mutations_list_server_revision
    ON mutations(list_id, server_revision);
