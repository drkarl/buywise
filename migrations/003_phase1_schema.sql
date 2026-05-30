-- ============================================================================
-- Migration 003: Phase 1 schema (households, lists, entries, sync, learning)
-- ============================================================================
-- Source of truth: docs/design.md §7, docs/implementation-plan.md (P1-T1).
-- Phase 1 introduces multi-user households, editable shopping lists, the
-- resolution lifecycle for list entries, an offline-sync mutation log, and the
-- recommendation/savings/feedback tables that capture learning signal.
-- Money is stored as INTEGER micros (1 EUR = 1_000_000) — never floats.
-- Timestamps are TEXT (ISO-8601 UTC, e.g. 2025-01-15T12:00:00Z).
-- Primary keys are TEXT (UUID or content hash strings).
-- ============================================================================

PRAGMA foreign_keys = ON;

-- A household is the top-level tenant that owns lists and members.
CREATE TABLE households (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

-- Individual people who can authenticate. A user may belong to many
-- households via household_members.
CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

-- Email is the login identity; enforce one account per address.
CREATE UNIQUE INDEX uq_users_email ON users(email);

-- Membership join table mapping users to households with a role.
CREATE TABLE household_members (
    id            TEXT PRIMARY KEY,
    household_id  TEXT NOT NULL,
    user_id       TEXT NOT NULL,
    role          TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id),
    FOREIGN KEY (user_id) REFERENCES users(id),
    CHECK (role IN ('owner', 'member'))
);

-- A user appears at most once per household.
CREATE UNIQUE INDEX uq_household_members_household_user
    ON household_members(household_id, user_id);

-- Authenticated sessions. token_hash is the SHA-256 of the opaque bearer
-- token (the raw token is never stored); csrf_token guards mutating requests.
CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL,
    token_hash  TEXT NOT NULL,
    csrf_token  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id)
);

-- Look up a session by the hash of the presented token.
CREATE UNIQUE INDEX uq_sessions_token_hash ON sessions(token_hash);
CREATE INDEX idx_sessions_user ON sessions(user_id);

-- A shopping list belongs to a household.
CREATE TABLE shopping_lists (
    id            TEXT PRIMARY KEY,
    household_id  TEXT NOT NULL,
    name          TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id)
);

CREATE INDEX idx_shopping_lists_household ON shopping_lists(household_id);

-- A line on a shopping list. Entries move through a resolution lifecycle:
--   unresolved  -> raw text, not yet matched to a canonical item
--   ambiguous   -> multiple candidate matches recorded for the user to pick
--   resolved    -> bound to exactly one canonical_item_id
-- The six CHECK implications below encode that lifecycle's invariants. They
-- MUST live inline in CREATE TABLE: SQLite cannot add CHECKs via ALTER TABLE.
--
-- NOTE on column naming: the issue body overloads `status` for both the
-- resolution enum (the six CHECKs) and the dedup index predicate's 'active'
-- value. Those are distinct concerns and cannot share one column, so the
-- resolution enum keeps `status` (as the six CHECKs require) and the soft-delete
-- lifecycle flag is `entry_status` ('active' | 'removed'); the partial unique
-- index predicate therefore reads `entry_status = 'active'`.
CREATE TABLE list_entries (
    id                          TEXT PRIMARY KEY,
    list_id                     TEXT NOT NULL,
    raw_text                    TEXT NOT NULL,
    status                      TEXT NOT NULL,
    canonical_item_id           TEXT,
    resolution_candidates_json  TEXT,
    quantity                    INTEGER NOT NULL DEFAULT 1,
    effective_constraint_hash   TEXT NOT NULL,
    entry_status                TEXT NOT NULL DEFAULT 'active',
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    FOREIGN KEY (list_id) REFERENCES shopping_lists(id),
    FOREIGN KEY (canonical_item_id) REFERENCES canonical_items(id),
    -- (1) resolution-status enum
    CHECK (status IN ('unresolved', 'ambiguous', 'resolved')),
    -- (2) resolved => a canonical item is bound
    CHECK (status != 'resolved' OR canonical_item_id IS NOT NULL),
    -- (3) a bound canonical item => status is resolved
    CHECK (canonical_item_id IS NULL OR status = 'resolved'),
    -- (4) unresolved => no candidate list recorded
    CHECK (status != 'unresolved' OR resolution_candidates_json IS NULL),
    -- (5) ambiguous => a non-empty candidate list is recorded
    CHECK (status != 'ambiguous' OR (
        resolution_candidates_json IS NOT NULL
        AND json_array_length(resolution_candidates_json) > 0
    )),
    -- (6) resolved => candidate list is cleared
    CHECK (status != 'resolved' OR resolution_candidates_json IS NULL)
);

CREATE INDEX idx_list_entries_list ON list_entries(list_id);

-- One active line per (list, canonical item, constraint hash). The predicate
-- scopes the constraint to active, resolved entries so historical/soft-deleted
-- rows and still-unresolved entries do not collide.
CREATE UNIQUE INDEX uq_list_entries_active_dedup
    ON list_entries(list_id, canonical_item_id, effective_constraint_hash)
    WHERE entry_status = 'active' AND canonical_item_id IS NOT NULL;

-- Append-only audit trail of changes to a list entry.
CREATE TABLE list_entry_events (
    id             TEXT PRIMARY KEY,
    list_entry_id  TEXT NOT NULL,
    event_type     TEXT NOT NULL,
    payload_json   TEXT,
    created_at     TEXT NOT NULL,
    FOREIGN KEY (list_entry_id) REFERENCES list_entries(id),
    CHECK (event_type IN (
        'created', 'resolved', 'reresolved', 'edited', 'removed'
    ))
);

CREATE INDEX idx_list_entry_events_entry ON list_entry_events(list_entry_id);

-- Offline-sync mutation log. Each client-originated mutation carries a stable
-- client_mutation_id so replays are idempotent on the server.
CREATE TABLE mutations (
    id                  TEXT PRIMARY KEY,
    client_mutation_id  TEXT NOT NULL,
    household_id        TEXT NOT NULL,
    user_id             TEXT NOT NULL,
    mutation_type       TEXT NOT NULL,
    payload_json        TEXT NOT NULL,
    applied_at          TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id),
    FOREIGN KEY (user_id) REFERENCES users(id)
);

-- Idempotency key: a replayed client mutation must not be applied twice.
CREATE UNIQUE INDEX uq_mutations_client_mutation_id
    ON mutations(client_mutation_id);
CREATE INDEX idx_mutations_household ON mutations(household_id);

-- Per-household, per-retailer aisle ordering of categories. Drives the
-- in-store walking order of a generated shopping route.
CREATE TABLE store_category_orders (
    id            TEXT PRIMARY KEY,
    household_id  TEXT NOT NULL,
    retailer      TEXT NOT NULL,
    category_id   TEXT NOT NULL,
    sort_order    INTEGER NOT NULL,
    created_at    TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id),
    FOREIGN KEY (category_id) REFERENCES categories(id),
    CHECK (retailer IN ('tesco', 'dunnes', 'supervalu', 'lidl', 'aldi'))
);

-- One ordering row per (household, retailer, category).
CREATE UNIQUE INDEX uq_store_category_orders_household_retailer_category
    ON store_category_orders(household_id, retailer, category_id);

-- A single execution of the recommendation engine for a list.
CREATE TABLE recommendation_runs (
    id                    TEXT PRIMARY KEY,
    household_id          TEXT NOT NULL,
    list_id               TEXT NOT NULL,
    strategy              TEXT NOT NULL,
    total_cost_micros     INTEGER NOT NULL,
    estimated_savings_micros INTEGER NOT NULL DEFAULT 0,
    result_json           TEXT NOT NULL,
    created_at            TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id),
    FOREIGN KEY (list_id) REFERENCES shopping_lists(id),
    CHECK (strategy IN ('cheapest_single_store', 'best_two_store', 'cheapest_overall'))
);

CREATE INDEX idx_recommendation_runs_list ON recommendation_runs(list_id);

-- Realised savings attributed to a recommendation run.
CREATE TABLE savings_events (
    id                     TEXT PRIMARY KEY,
    household_id           TEXT NOT NULL,
    recommendation_run_id  TEXT NOT NULL,
    savings_micros         INTEGER NOT NULL,
    created_at             TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id),
    FOREIGN KEY (recommendation_run_id) REFERENCES recommendation_runs(id)
);

CREATE INDEX idx_savings_events_household ON savings_events(household_id);

-- User feedback on a proposed canonical-item match, used to improve resolution.
CREATE TABLE match_feedback (
    id                 TEXT PRIMARY KEY,
    household_id       TEXT NOT NULL,
    list_entry_id      TEXT NOT NULL,
    canonical_item_id  TEXT NOT NULL,
    verdict            TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    FOREIGN KEY (household_id) REFERENCES households(id),
    FOREIGN KEY (list_entry_id) REFERENCES list_entries(id),
    FOREIGN KEY (canonical_item_id) REFERENCES canonical_items(id),
    CHECK (verdict IN ('accepted', 'rejected'))
);

CREATE INDEX idx_match_feedback_entry ON match_feedback(list_entry_id);
