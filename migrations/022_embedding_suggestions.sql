-- ============================================================================
-- Migration 022: Embedding-based suggestions for unmatched imported lines
-- (follow-up of the Tesco import #107, roadmap #114)
-- ============================================================================
-- Thousands of imported Tesco lines stay `unmatched` because the deterministic
-- cascade is lexical: a branded SKU (`Tesco Free Range Eggs Large Box Of 12`)
-- rarely contains a catalogue staple verbatim, and the catalogue itself is 227
-- generic staples. The embedding bake-off showed a text embedding model ranks
-- the correct staple in its top five for ~79.5% of the residual names, and that
-- this is best used to RANK a clarification shortlist, never to auto-merge.
--
-- This migration adds the two vector stores that feature needs. It is purely
-- additive: two new tables, no change to any existing table.
--
--   * `catalogue_embeddings` — one vector per embedded catalogue string, i.e.
--     per canonical item `name_en` / `name_es` and per `item_aliases` row. The
--     embedded text is stored verbatim so a re-embed of a changed catalogue can
--     be diffed, and `(canonical_item_id, text, model)` is unique so re-running
--     the batch step is idempotent. Ranking a query against the catalogue takes
--     the best (max cosine) string per item, so an alias hit and a name hit for
--     the same item collapse to one candidate.
--
--   * `line_name_embeddings` — one vector per distinct unmatched imported line
--     name, keyed by the name text. Imported line names are household data and
--     are never copied into the repository; only the aggregate counts are
--     reported by the operator commands.
--
-- `vector` is a BLOB of little-endian `f32` components (4 bytes each); `dim`
-- records the component count so a reader can reject a vector written by an
-- incompatible model. `model` tags the vectors so switching models leaves the
-- old rows inert rather than silently mixing vector spaces: every query filters
-- on the configured model id.
--
-- Conventions (match 019_purchase_ledger.sql / 021_purchase_manual_entry.sql):
--   * TEXT primary keys; timestamps are TEXT (ISO-8601 UTC).
--   * Enum-like columns carry an explicit CHECK(... IN (...)).
--   * No auto-merge: nothing in the schema links a `purchase_lines` row to a
--     canonical item except the existing `canonical_item_id` column, which only
--     the import (deterministic cascade) and an explicit user confirmation write.
-- ============================================================================

PRAGMA foreign_keys = ON;

CREATE TABLE catalogue_embeddings (
    id                TEXT PRIMARY KEY,
    household_id      TEXT NOT NULL REFERENCES households(id),
    canonical_item_id TEXT NOT NULL REFERENCES canonical_items(id) ON DELETE CASCADE,
    -- The catalogue string that was embedded (a name or an alias), verbatim.
    text              TEXT NOT NULL,
    -- Model identifier the vector was produced by; a query only compares
    -- vectors of its own model.
    model             TEXT NOT NULL,
    -- Component count of `vector` (768 for EmbeddingGemma 2's text part).
    dim               INTEGER NOT NULL CHECK(dim > 0),
    -- Little-endian f32 components, 4 bytes each: dim * 4 bytes.
    vector            BLOB NOT NULL,
    created_at        TEXT NOT NULL
);

CREATE UNIQUE INDEX uq_catalogue_embeddings_item_text_model
    ON catalogue_embeddings(household_id, canonical_item_id, text, model);

CREATE INDEX idx_catalogue_embeddings_household_model
    ON catalogue_embeddings(household_id, model);

CREATE TABLE line_name_embeddings (
    id           TEXT PRIMARY KEY,
    household_id TEXT NOT NULL REFERENCES households(id),
    -- The distinct imported line name that was embedded, verbatim.
    name         TEXT NOT NULL,
    model        TEXT NOT NULL,
    dim          INTEGER NOT NULL CHECK(dim > 0),
    vector       BLOB NOT NULL,
    created_at   TEXT NOT NULL
);

CREATE UNIQUE INDEX uq_line_name_embeddings_household_name_model
    ON line_name_embeddings(household_id, name, model);
