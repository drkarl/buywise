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
sqlite3 "$DB" <<SQL
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

echo "seed.sh: seeded user=$EMAIL list=$LIST_ID into $DB"
