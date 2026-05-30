-- ============================================================================
-- Migration 003: Phase 1 schema (households, lists, entries, sync, learning)
-- ============================================================================
-- Source of truth: docs/implementation-plan.md (P1-T1) and docs/design.md §7.
-- The column layout below follows the canonical schema in the implementation
-- plan verbatim; the Rust enums that back the TEXT columns live in
-- crates/core/src/domain.rs (ResolutionStatus, ItemStatus, Retailer, ...).
--
-- Phase 1 introduces multi-user households, editable shopping lists, the
-- resolution lifecycle for list entries, an offline-sync mutation log with a
-- per-list revision counter, and the recommendation/savings/feedback tables
-- that capture learning signal.
--
-- Conventions (match 001_phase0_schema.sql):
--   * Primary keys are TEXT (UUID or content-hash strings).
--   * Timestamps are TEXT (ISO-8601 UTC, e.g. 2025-01-15T12:00:00Z).
--   * Money is INTEGER cents (recommendation/savings tables) — never floats.
--   * Enum-like columns carry an explicit CHECK(... IN (...)) whose value set
--     mirrors the corresponding domain.rs enum's `as_db_str` output.
--   * api_usage_events is defined in 001 (Phase 0); it is NOT redefined here.
-- ============================================================================

PRAGMA foreign_keys = ON;

-- A household is the top-level tenant that owns lists and members.
CREATE TABLE households (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

-- Individual people who can authenticate. A user may belong to many households
-- via household_members. password_hash stores the argon2id hash; it is part of
-- the initial table because SQLite cannot add a NOT NULL column via ALTER once
-- rows exist (the MVP is password login — see P1-T2).
CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

-- Membership join table mapping users to households. Composite PK: a user
-- appears at most once per household.
CREATE TABLE household_members (
    household_id TEXT NOT NULL REFERENCES households(id),
    user_id      TEXT NOT NULL REFERENCES users(id),
    role         TEXT NOT NULL DEFAULT 'member' CHECK(role IN ('owner', 'member')),
    joined_at    TEXT NOT NULL,
    PRIMARY KEY (household_id, user_id)
);

-- Authenticated sessions. token_hash is the SHA-256 of the opaque bearer token
-- (the raw token is never stored); csrf_token guards mutating requests.
CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,   -- opaque random token used as the cookie value
    user_id     TEXT NOT NULL REFERENCES users(id),
    token_hash  TEXT NOT NULL UNIQUE,   -- SHA-256 of the session token
    csrf_token  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL
);

CREATE INDEX idx_sessions_token_hash ON sessions(token_hash);
CREATE INDEX idx_sessions_expires_at ON sessions(expires_at);

-- A shopping list belongs to a household. current_revision is the per-list
-- counter the offline-sync protocol increments on every accepted mutation;
-- clients send their base_revision and the server detects staleness against it.
CREATE TABLE shopping_lists (
    id                TEXT PRIMARY KEY,
    household_id      TEXT NOT NULL REFERENCES households(id),
    name              TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active', 'archived')),
    current_revision  INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL,
    archived_at       TEXT
);

CREATE INDEX idx_shopping_lists_household ON shopping_lists(household_id);

-- A line on a shopping list. Two orthogonal state columns:
--   * resolution_status (unresolved | resolved | ambiguous) — the matching
--     lifecycle, governed by the six CHECK implications below.
--   * status (active | bought | skipped | deleted) — the item lifecycle, used by
--     the dedup index predicate (WHERE status = 'active').
-- These map to domain.rs ResolutionStatus and ItemStatus respectively. The six
-- resolution CHECKs MUST live inline: SQLite cannot add CHECKs via ALTER TABLE.
-- NOTE: SQLite requires every column definition to precede the table-level
-- constraints, so the six resolution CHECKs are grouped at the end rather than
-- interleaved with the columns (as the plan lists them illustratively). They are
-- still inline in CREATE TABLE, which is what matters — SQLite cannot add CHECKs
-- via ALTER TABLE.
CREATE TABLE list_entries (
    id                          TEXT PRIMARY KEY,   -- client-generated UUID v4
    list_id                     TEXT NOT NULL REFERENCES shopping_lists(id),
    canonical_item_id           TEXT REFERENCES canonical_items(id),  -- NULL until resolved
    resolution_status           TEXT NOT NULL DEFAULT 'unresolved'
        CHECK(resolution_status IN ('unresolved', 'resolved', 'ambiguous')),
    resolution_candidates_json  TEXT,               -- JSON array of canonical_item_ids; ambiguous only
    raw_text                    TEXT NOT NULL,
    display_name                TEXT NOT NULL,
    added_by                    TEXT NOT NULL REFERENCES users(id),
    quantity                    INTEGER NOT NULL DEFAULT 1,
    status                      TEXT NOT NULL DEFAULT 'active'
        CHECK(status IN ('active', 'bought', 'skipped', 'deleted')),
    bought_by                   TEXT REFERENCES users(id),
    bought_at                   TEXT,
    -- sentinel 'unresolved:<entry_id>' when canonical_item_id IS NULL;
    -- sentinel 'default' when resolved with an empty constraint set;
    -- otherwise SHA-256 hex of the sorted-key JSON of the resolved constraints.
    effective_constraint_hash   TEXT NOT NULL,
    last_updated_revision       INTEGER NOT NULL DEFAULT 0,
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    -- resolved => a canonical item is bound
    CHECK(resolution_status != 'resolved' OR canonical_item_id IS NOT NULL),
    -- canonical_item_id bound => status is resolved
    CHECK(canonical_item_id IS NULL OR resolution_status = 'resolved'),
    -- unresolved => no candidate list recorded
    CHECK(resolution_status != 'unresolved' OR resolution_candidates_json IS NULL),
    -- ambiguous => a candidate list is recorded (well-formed JSON)
    CHECK(resolution_status != 'ambiguous'
          OR (resolution_candidates_json IS NOT NULL
              AND json_valid(resolution_candidates_json))),
    -- ambiguous => the candidate list is a non-empty array
    CHECK(resolution_status != 'ambiguous'
          OR json_array_length(resolution_candidates_json) > 0),
    -- resolved => candidate list is cleared
    CHECK(resolution_status != 'resolved' OR resolution_candidates_json IS NULL)
);

CREATE INDEX idx_list_entries_list ON list_entries(list_id);

-- One active line per (list, canonical item, constraint hash). The predicate
-- scopes the constraint to active, resolved entries so bought/skipped/deleted
-- rows and still-unresolved entries (canonical_item_id IS NULL) do not collide.
CREATE UNIQUE INDEX uq_list_entries_active_dedup
    ON list_entries(list_id, canonical_item_id, effective_constraint_hash)
    WHERE status = 'active' AND canonical_item_id IS NOT NULL;

-- Append-only audit trail of changes to a list entry.
CREATE TABLE list_entry_events (
    id          TEXT PRIMARY KEY,
    entry_id    TEXT NOT NULL REFERENCES list_entries(id),
    user_id     TEXT NOT NULL REFERENCES users(id),
    event_type  TEXT NOT NULL,
    payload     TEXT,    -- JSON
    created_at  TEXT NOT NULL
);

CREATE INDEX idx_list_entry_events_entry ON list_entry_events(entry_id);

-- Offline-sync mutation log. Each client-originated mutation carries a stable
-- client_mutation_id (idempotency key) and is ordered per list by
-- server_revision. response_json is stored so a duplicate client_mutation_id
-- replay can return the original response without re-applying.
CREATE TABLE mutations (
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
                                 'reject_match', 'set_constraint')),
    payload             TEXT NOT NULL,           -- JSON
    base_revision       INTEGER NOT NULL,
    created_at_client   TEXT NOT NULL,
    received_at_server  TEXT NOT NULL,
    server_revision     INTEGER,                 -- NULL until accepted
    status              TEXT NOT NULL DEFAULT 'accepted'
        CHECK(status IN ('accepted', 'rejected', 'merged')),
    response_json       TEXT                     -- stored response for idempotent replay
);

-- Per-list ordering of accepted mutations (also drives stale-revision detection).
CREATE INDEX idx_mutations_list_server_revision
    ON mutations(list_id, server_revision);

-- Per-household, per-store aisle ordering of categories. Drives the in-store
-- walking order of a generated shopping route. `store` mirrors domain.rs
-- Retailer::as_db_str.
CREATE TABLE store_category_orders (
    id            TEXT PRIMARY KEY,
    household_id  TEXT NOT NULL REFERENCES households(id),
    store         TEXT NOT NULL CHECK(store IN ('tesco', 'dunnes', 'supervalu', 'aldi', 'lidl')),
    category_id   TEXT NOT NULL REFERENCES categories(id),
    display_order INTEGER NOT NULL,
    updated_at    TEXT NOT NULL,
    UNIQUE(household_id, store, category_id)
);

-- A single execution of the recommendation engine for a list. plans_json holds
-- all four plans; thresholds are in cents.
CREATE TABLE recommendation_runs (
    id                       TEXT PRIMARY KEY,
    list_id                  TEXT NOT NULL REFERENCES shopping_lists(id),
    household_id             TEXT NOT NULL REFERENCES households(id),
    run_at                   TEXT NOT NULL,
    offer_snapshot_date      TEXT,
    plans_json               TEXT NOT NULL,
    recommended_plan         TEXT NOT NULL,
    convenience_threshold_2  INTEGER NOT NULL,    -- cents
    convenience_threshold_3  INTEGER NOT NULL     -- cents
);

CREATE INDEX idx_recommendation_runs_list ON recommendation_runs(list_id);

-- Realised savings attributed to a recommendation run. All money in cents.
CREATE TABLE savings_events (
    id                             TEXT PRIMARY KEY,
    list_id                        TEXT NOT NULL REFERENCES shopping_lists(id),
    recommendation_run_id          TEXT REFERENCES recommendation_runs(id),
    baseline_cost_cents            INTEGER NOT NULL,
    recommended_cost_cents         INTEGER NOT NULL,
    theoretical_minimum_cost_cents INTEGER NOT NULL,
    chosen_basket_cost_cents       INTEGER,    -- manual entry after the shop
    chosen_plan                    TEXT,
    chosen_stores                  TEXT,       -- JSON array
    gross_saving_cents             INTEGER NOT NULL,
    accepted_savings_cents         INTEGER,
    rejected_savings_cents         INTEGER,
    travel_cost_cents              INTEGER,
    subscription_cost_alloc_cents  INTEGER,
    net_saving_cents               INTEGER,
    created_at                     TEXT NOT NULL
);

CREATE INDEX idx_savings_events_list ON savings_events(list_id);

-- User feedback on a proposed canonical-item match, used to improve resolution.
-- References product_matches(id) from the Phase 0 schema (001).
CREATE TABLE match_feedback (
    id                TEXT PRIMARY KEY,
    product_match_id  TEXT NOT NULL REFERENCES product_matches(id),
    user_id           TEXT NOT NULL REFERENCES users(id),
    action            TEXT NOT NULL CHECK(action IN ('accept', 'reject', 'prefer')),
    created_at        TEXT NOT NULL
);

CREATE INDEX idx_match_feedback_product_match ON match_feedback(product_match_id);
