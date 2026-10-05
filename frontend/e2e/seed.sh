#!/usr/bin/env bash
# Seed a fresh BuyWise database for the Playwright e2e suite.
#
# The backend has no public registration endpoint — auth is password login only
# (crates/api/src/auth.rs). So the suite seeds a household, a member user, and a
# shopping list directly, then sets the user's argon2id password hash with the
# `buywise-cli set-password` tool (the same tool operators use). The two browser
# contexts in the specs both log in as this one user (same household + list), so
# their edits sync to each other exactly as two real devices would.
#
# Idempotent: every row is INSERT OR REPLACE / OR IGNORE so re-seeding an
# existing DB (e.g. a re-run against a persisted file) is harmless. The IDs and
# credentials are fixed and mirrored in `e2e/fixtures.ts`.
#
# Env (all optional, defaults match the harness):
#   E2E_DB        path to the SQLite file the server uses     (default: e2e.db)
#   E2E_CLI       path to the buywise-cli binary              (default: release build)
#   E2E_EMAIL / E2E_PASSWORD / E2E_LIST_ID / E2E_HOUSEHOLD_ID / E2E_USER_ID
#
# The DB must already have the schema applied. The server runs migrations on
# boot, so this script is invoked AFTER the server is up (see run-server.sh).
set -euo pipefail

DB="${E2E_DB:-e2e.db}"
EMAIL="${E2E_EMAIL:-e2e@buywise.test}"
PASSWORD="${E2E_PASSWORD:-e2e-password-123}"
LIST_ID="${E2E_LIST_ID:-list-e2e}"
HOUSEHOLD_ID="${E2E_HOUSEHOLD_ID:-hh-e2e}"
USER_ID="${E2E_USER_ID:-user-e2e}"

# Resolve the CLI: explicit override, else the workspace release build.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CLI="${E2E_CLI:-$REPO_ROOT/target/release/buywise-cli}"

if [[ ! -f "$DB" ]]; then
  echo "seed.sh: database $DB does not exist (is the server up and migrated?)" >&2
  exit 1
fi
if [[ ! -x "$CLI" ]]; then
  echo "seed.sh: buywise-cli not found/executable at $CLI" >&2
  echo "         build it with: cargo build --release -p buywise-cli" >&2
  exit 1
fi

NOW="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"

# Insert the household, user (placeholder hash — set below), membership, list.
# OR IGNORE so a re-seed against the same file is a no-op rather than an error.
#
# The values interpolated into this heredoc come from FIXED, trusted dev-only
# constants (the defaults above, or env overrides the harness sets) — never
# untrusted input — so this is not an injection surface. run-server.sh invokes
# this with NO server running (it seeds between the migrate-only phase and the
# real server start), so there is normally no contention; busy_timeout is cheap
# insurance against a transient SQLITE_BUSY when seeding a file run-server.sh just
# released, or when re-seeding a DB another process holds.
sqlite3 "$DB" <<SQL
PRAGMA busy_timeout=5000;
INSERT OR IGNORE INTO households (id, name, created_at)
  VALUES ('$HOUSEHOLD_ID', 'E2E Household', '$NOW');
INSERT OR IGNORE INTO users (id, email, password_hash, display_name, created_at)
  VALUES ('$USER_ID', '$EMAIL', 'PLACEHOLDER', 'E2E User', '$NOW');
INSERT OR IGNORE INTO household_members (household_id, user_id, role, joined_at)
  VALUES ('$HOUSEHOLD_ID', '$USER_ID', 'owner', '$NOW');
INSERT OR IGNORE INTO shopping_lists (id, household_id, name, status, current_revision, created_at)
  VALUES ('$LIST_ID', '$HOUSEHOLD_ID', 'E2E List', 'active', 0, '$NOW');
SQL

# Set the real argon2id hash via the operator tool (reads the password from
# stdin; the plaintext is never a CLI argument).
printf '%s\n' "$PASSWORD" | "$CLI" set-password "$EMAIL" --db "$DB" >/dev/null

# --- Per-store trip fixture (issue #104) -----------------------------------
# Seed a tiny, deterministic two-store basket so the optimiser returns a real
# two-store plan: Milk is far cheaper at Aldi, Bread far cheaper at Tesco, so
# the split clears the €10 convenience threshold. A third, UNRESOLVED entry
# exercises the trip view's explicit "not allocated" section. The list entries
# are inserted already RESOLVED (with their canonical ids) so the plan does not
# depend on the resolution cascade, keeping the e2e deterministic.
# `store_category_orders` is not seeded for this household (migration 004 seeds
# only the default household); category grouping falls back to each category's
# default_order, which is fine for the two categories used here.
sqlite3 "$DB" <<SQL
PRAGMA busy_timeout=5000;
INSERT OR REPLACE INTO canonical_items
  (id, household_id, name_en, name_es, category_id, default_constraint_policy, created_at)
VALUES
  ('e2e-ci-milk',  '$HOUSEHOLD_ID', 'Milk',  'Leche',  'c1000000-0000-0000-0000-000000000003', 'any', '$NOW'),
  ('e2e-ci-bread', '$HOUSEHOLD_ID', 'Bread', 'Pan',    'c1000000-0000-0000-0000-000000000004', 'any', '$NOW');

INSERT OR REPLACE INTO retailer_products
  (id, source, retailer, external_id, name, pack_count, first_seen_at, last_seen_at)
VALUES
  ('e2e-rp-milk-aldi',  'manual', 'aldi',  NULL, 'Brooklea Whole Milk 2L', 1, '$NOW', '$NOW'),
  ('e2e-rp-milk-tesco', 'manual', 'tesco', NULL, 'Tesco Whole Milk 2L',    1, '$NOW', '$NOW'),
  ('e2e-rp-bread-aldi', 'manual', 'aldi',  NULL, 'Ballymore Bread 800g',   1, '$NOW', '$NOW'),
  ('e2e-rp-bread-tesco','manual', 'tesco', NULL, 'Tesco White Bread 800g', 1, '$NOW', '$NOW');

INSERT OR REPLACE INTO product_offers
  (id, retailer_product_id, price_cents, source_snapshot_date, fetched_at, source_confidence_at_ingest)
VALUES
  ('e2e-of-milk-aldi',   'e2e-rp-milk-aldi',   100,  '2026-07-01', '$NOW', 'high'),
  ('e2e-of-milk-tesco',  'e2e-rp-milk-tesco',  1500, '2026-07-01', '$NOW', 'high'),
  ('e2e-of-bread-aldi',  'e2e-rp-bread-aldi',  1300, '2026-07-01', '$NOW', 'high'),
  ('e2e-of-bread-tesco', 'e2e-rp-bread-tesco', 150,  '2026-07-01', '$NOW', 'high');

INSERT OR REPLACE INTO product_matches
  (id, household_id, canonical_item_id, retailer_product_id, status, matched_by, created_at, updated_at)
VALUES
  ('e2e-match-milk-aldi',   '$HOUSEHOLD_ID', 'e2e-ci-milk',  'e2e-rp-milk-aldi',   'accepted', 'system', '$NOW', '$NOW'),
  ('e2e-match-milk-tesco',  '$HOUSEHOLD_ID', 'e2e-ci-milk',  'e2e-rp-milk-tesco',  'accepted', 'system', '$NOW', '$NOW'),
  ('e2e-match-bread-aldi',  '$HOUSEHOLD_ID', 'e2e-ci-bread', 'e2e-rp-bread-aldi',  'accepted', 'system', '$NOW', '$NOW'),
  ('e2e-match-bread-tesco', '$HOUSEHOLD_ID', 'e2e-ci-bread', 'e2e-rp-bread-tesco', 'accepted', 'system', '$NOW', '$NOW');

INSERT OR REPLACE INTO list_entries
  (id, list_id, canonical_item_id, resolution_status, raw_text, display_name, added_by,
   quantity, status, effective_constraint_hash, last_updated_revision, created_at, updated_at)
VALUES
  ('e2e-entry-milk',  '$LIST_ID', 'e2e-ci-milk',  'resolved',   'Milk',  'Milk',  '$USER_ID', 2, 'active', 'default', 0, '$NOW', '$NOW'),
  ('e2e-entry-bread', '$LIST_ID', 'e2e-ci-bread', 'resolved',   'Bread', 'Bread', '$USER_ID', 1, 'active', 'default', 0, '$NOW', '$NOW'),
  ('e2e-entry-mystery', '$LIST_ID', NULL, 'unresolved', 'Mystery sauce', 'Mystery sauce', '$USER_ID', 1, 'active', 'unresolved:e2e-entry-mystery', 0, '$NOW', '$NOW');
SQL

echo "seed.sh: seeded two-store trip fixture into $DB"
