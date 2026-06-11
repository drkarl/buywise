-- ============================================================================
-- Migration 009: Add `move_to_list` to the mutations.operation_type CHECK
-- ============================================================================
-- Source of truth: docs/design.md §11 (mutation protocol). Adds a syncable
-- operation that moves a `bought` or `skipped` entry back to `active` — the
-- "undo I checked this off / skipped it by mistake" action — so the reversal
-- survives a full resnapshot/sync like any other mutation. The canonical
-- `operation_type` enum gains one value:
--
--   add_item | mark_bought | mark_skipped | delete_item | rename_item
--   | resolve_alias | reject_match | set_constraint | set_quantity
--   | move_to_list                                                   <-- new
--
-- `move_to_list` carries an empty payload (`{}`): the target is identified by
-- entity_id and the effect is absolute (status -> 'active', bought_by/bought_at
-- cleared), so replays under last-write-wins are idempotent. The handler
-- (`apply_move_to_list`) enforces the "delete wins" rule — a deleted entry is
-- never resurrected — so the column-level CHECK stays minimal here.
--
-- SQLite cannot add/alter a column CHECK via ALTER TABLE, so the constraint is
-- changed the only supported way: build a new table with the widened CHECK, copy
-- the rows, drop the old table, and rename. The `mutations` table is append-only
-- (idempotency log), so the copy is a straight `INSERT ... SELECT` with no
-- transformation. Column order/types/constraints below are copied verbatim from
-- 008_mutations_add_set_quantity.sql except for the one widened CHECK.
--
-- Conventions (match 003_phase1_schema.sql):
--   * Primary keys are TEXT.
--   * Timestamps are TEXT (ISO-8601 UTC).
--   * Enum-like columns carry an explicit CHECK(... IN (...)).
-- ============================================================================

PRAGMA foreign_keys = ON;

-- 1. New table with the widened operation_type CHECK (everything else verbatim).
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
                                 'move_to_list')),
    payload             TEXT NOT NULL,           -- JSON
    base_revision       INTEGER NOT NULL,
    created_at_client   TEXT NOT NULL,
    received_at_server  TEXT NOT NULL,
    server_revision     INTEGER,                 -- NULL until accepted
    status              TEXT NOT NULL DEFAULT 'accepted'
        CHECK(status IN ('accepted', 'rejected', 'merged')),
    response_json       TEXT                     -- stored response for idempotent replay
);

-- 2. Copy existing rows verbatim (append-only log; no transformation needed).
INSERT INTO mutations_new
    SELECT id, client_mutation_id, device_id, user_id, list_id, entity_type,
           entity_id, operation_type, payload, base_revision, created_at_client,
           received_at_server, server_revision, status, response_json
    FROM mutations;

-- 3. Swap the tables.
DROP TABLE mutations;
ALTER TABLE mutations_new RENAME TO mutations;

-- 4. Recreate the index dropped with the old table (verbatim from 003).
CREATE INDEX idx_mutations_list_server_revision
    ON mutations(list_id, server_revision);
