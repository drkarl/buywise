-- ============================================================================
-- Migration 017: Enforce the household owner role (issue #139, roadmap #114)
-- ============================================================================
-- `household_members.role` (`owner` / `member`, migration 003) was stored but
-- never enforced: any member could export and delete the household. The member
-- administration screen needs a real owner, so the earliest-joined member of
-- every household is promoted, giving each pre-existing household exactly one
-- owner. Households created after this migration get an owner from
-- `bootstrap-household` / the add-member API, which both write role `owner` for
-- the creator and `member` for everyone added afterwards.
--
-- Additive only / no rebuild: a single UPDATE, no table rewrite, no column
-- change. It is idempotent in effect — once a household has an owner this
-- matches nothing — so re-running is harmless. A household with no members has
-- no row to promote and stays empty.
--
-- Tie-break: `joined_at` is not necessarily unique (seeded/imported rows can
-- share a timestamp), so `user_id` breaks ties deterministically.

UPDATE household_members
SET role = 'owner'
WHERE role <> 'owner'
  -- Only households that have no owner at all.
  AND NOT EXISTS (
      SELECT 1 FROM household_members owner
      WHERE owner.household_id = household_members.household_id
        AND owner.role = 'owner'
  )
  -- The earliest-joined member of that household (deterministic tie-break).
  AND user_id = (
      SELECT candidate.user_id FROM household_members candidate
      WHERE candidate.household_id = household_members.household_id
      ORDER BY candidate.joined_at ASC, candidate.user_id ASC
      LIMIT 1
  );
