-- ============================================================================
-- Migration 018: Frozen recommendation-run evidence snapshot (issue #117,
-- roadmap #114; slice 2 of the savings methodology)
-- ============================================================================
-- Slice 1 (migration 016) made savings labels honest and the aggregates
-- verified-only. This slice makes every recommendation freeze the evidence it
-- was computed from, so a later comparison against what was actually bought can
-- never be rebuilt from mutable `list_entries` rows or from offers refreshed
-- since the run.
--
-- Six additive, nullable columns on `recommendation_runs`:
--
--   * `basket_snapshot_json`  — the input basket as the optimiser saw it: one
--     entry per ACTIVE list line with its `list_entries.id`, canonical item id,
--     display text, requested pack count, constraint policy, product rules
--     (`item_constraints`) and the constrained base-unit size where known.
--   * `price_evidence_json`   — one row per offer the run used (the offers
--     backing the best-single and recommended plans): offer id, retailer,
--     product id/name, observed `source_snapshot_date`, shelf and loyalty price,
--     loyalty scheme, unit price, pack size, and the discount assumptions
--     (`previous_price_cents`, `offer_text`, `offer_valid_until`).
--   * `coverage_json`         — priced / missing / unresolved /
--     confirmation-needed item indices plus the common-scope line set priced by
--     BOTH the best-single and the recommended plan (and the exclusive lines on
--     each side).
--   * `usual_profile_json`    — RESERVED for the later usual-shop quotes
--     (issue #109). Always NULL in this slice; the household's written
--     usual-shop profile does not exist yet, so this only leaves room for it.
--   * `snapshot_version`      — the snapshot schema version (currently 1), so a
--     later reader can tell which layout a row carries.
--   * `snapshot_digest`       — SHA-256 hex over the three JSON payloads, a
--     tamper/immutability check for the frozen evidence.
--
-- **Offer snapshot date (meaning changed).** `offer_snapshot_date` now records
-- the OLDEST `source_snapshot_date` among the offers the run used, not the
-- latest. A latest date overstates freshness: a basket priced from a mixture of
-- evidence compiles to the age of its stalest line. The column is unchanged and
-- existing consumers keep the same field name.
--
-- **No rebuild / readable old runs.** `ALTER TABLE ... ADD COLUMN` appends a
-- column (SQLite never rewrites the table), every column is nullable, and no
-- existing column is dropped or retyped. Runs written before this migration keep
-- NULL snapshot columns and stay readable everywhere (the coverage read path
-- treats a NULL snapshot as "no frozen evidence").
-- ============================================================================

PRAGMA foreign_keys = ON;

ALTER TABLE recommendation_runs ADD COLUMN snapshot_version      INTEGER;
ALTER TABLE recommendation_runs ADD COLUMN basket_snapshot_json  TEXT;
ALTER TABLE recommendation_runs ADD COLUMN price_evidence_json   TEXT;
ALTER TABLE recommendation_runs ADD COLUMN coverage_json         TEXT;
ALTER TABLE recommendation_runs ADD COLUMN usual_profile_json    TEXT;
ALTER TABLE recommendation_runs ADD COLUMN snapshot_digest       TEXT;
