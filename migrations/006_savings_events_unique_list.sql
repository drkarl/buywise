-- ============================================================================
-- Migration 006: One savings_events row per list (UNIQUE list_id)
-- ============================================================================
-- Source of truth: docs/implementation-plan.md (P5-T3) and docs/design.md
-- §savings. A list produces at most ONE savings event over its lifetime — it is
-- created when the list is first archived (see lists::record_savings_event) and
-- later updated in place when the household posts actual spend
-- (savings::record_actual_spend). 003_phase1_schema.sql indexed
-- savings_events(list_id) but did NOT make it unique, so the archive path's
-- read-then-insert idempotency guard was racy: two concurrent archives of the
-- same list could both see no row and both insert, duplicating the event.
--
-- This migration enforces the invariant structurally by replacing the plain
-- index with a UNIQUE one, which lets record_savings_event use
-- `INSERT ... ON CONFLICT(list_id) DO NOTHING` for an atomic, race-free upsert.
-- The UNIQUE index still backs the by-list lookups the plain index served, so
-- nothing is lost.
--
-- Conventions (match 003_phase1_schema.sql):
--   * Primary keys are TEXT.
--   * Timestamps are TEXT (ISO-8601 UTC).
--   * Money is INTEGER cents — never floats.
--
-- Safety: savings_events is a brand-new write path introduced in this same phase
-- (P5-T3), so no deployed database can already hold duplicate list_id rows; the
-- swap below cannot fail on pre-existing data.
-- ============================================================================

PRAGMA foreign_keys = ON;

DROP INDEX IF EXISTS idx_savings_events_list;

CREATE UNIQUE INDEX idx_savings_events_list ON savings_events(list_id);
