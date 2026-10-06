-- ============================================================================
-- Migration 016: Honest savings-event status (issue #117, roadmap #114)
-- ============================================================================
-- The savings ledger stored a gross figure (baseline − recommended) for every
-- archived list without checking that the best single store and the recommended
-- plan priced the same lines, and it stored a whole-basket manual actual spend
-- as a "net saving" against that estimate. Both overstate what the row proves:
-- buying fewer items would read as a saving.
--
-- This adds a `status` label so the summary can aggregate only reconciled,
-- verified rows and show everything else under its own honest label:
--   * `verified`          — line- and quantity-reconciled evidence. RESERVED:
--                           never produced by this slice (needs the purchase
--                           ledger of #116), so the verified aggregate is empty
--                           today.
--   * `estimated`         — a comparable-coverage gross estimate (both plans
--                           price the same lines) with no actual spend yet.
--   * `unreconciled`      — the plans price different lines, so no saving is
--                           claimed; the stored gross figure is kept for audit
--                           but never presented as a saving. Also the backfill
--                           for pre-guard rows without an actual-spend total,
--                           whose comparability was never checked.
--   * `legacy_total_only` — a manual whole-basket actual-spend total compared
--                           against the best single store estimate. Quantities
--                           and purchases are unverified; travel is included
--                           where entered; service and data costs are excluded.
--
-- Safety / no rebuild: `ALTER TABLE ... ADD COLUMN` appends a column (SQLite
-- never rewrites the table here) and the backfill is a single UPDATE. No
-- existing column is dropped or retyped, so every existing row stays readable.
-- SQLite cannot add a CHECK constraint via ALTER, so the value set is enforced
-- by the writers (`lists::record_savings_event`, `savings::record_actual_spend`).
-- ============================================================================

PRAGMA foreign_keys = ON;

ALTER TABLE savings_events ADD COLUMN status TEXT NOT NULL DEFAULT 'unreconciled';

-- Pre-guard rows: a row with a manual actual-spend total is the legacy
-- total-only basket variance; one without was never coverage-checked, so it
-- stays `unreconciled` (the ALTER's default).
UPDATE savings_events
   SET status = 'legacy_total_only'
 WHERE chosen_basket_cost_cents IS NOT NULL;
