#!/usr/bin/env bash
# Boot the BuyWise backend for the Playwright e2e suite and seed it.
#
# The Axum server serves BOTH the JSON API under `/api/*` AND the built
# SvelteKit SPA from `frontend/build` on a single origin (crates/api/src/lib.rs
# `app()` -> `ServeDir` fallback). Running just this one binary therefore gives
# the e2e a same-origin app: the `Secure; SameSite=Strict` session/csrf cookies
# work over http://localhost (Chromium treats localhost as a secure context),
# and there is no proxy/CORS to configure — which is why the suite drives the
# backend directly rather than `vite preview` (Vite has no API proxy; see
# vite.config.ts).
#
# Order of operations. Playwright's webServer readiness probe (GET / on PORT)
# must only go green AFTER the DB is migrated AND seeded — otherwise tests can
# start logging in before the seed user/list exist. The Axum ServeDir serves the
# SPA shell with a 200 the instant the server is listening (independent of DB
# state), so we MUST NOT let the real server bind PORT until seeding is done.
# Hence a two-phase boot, with all DB writes happening while PORT is unbound:
#   1. delete any stale e2e DB so every run starts from clean, known state;
#   2. PHASE 1 — start a THROWAWAY server on a separate ephemeral port purely to
#      run migrations (buywise-api migrates on boot), then kill it. PORT is never
#      bound in this phase, so Playwright cannot latch onto the transient listener;
#   3. seed.sh against the now-migrated file while NO server is running (no WAL
#      contention, fully deterministic);
#   4. PHASE 2 — start the REAL server on PORT in the foreground. By the time `/`
#      first answers 200 on PORT, the file is already migrated AND seeded, so the
#      Playwright readiness probe going green is a true go-signal. Playwright owns
#      this process's lifecycle and tears it down after the run.
#
# Env:
#   PORT        port to serve on                 (default: 3000; webServer matches)
#   E2E_DB      SQLite file path                 (default: e2e.db, gitignored)
#   E2E_API     path to the buywise-api binary   (default: release build)
#   E2E_MIGRATE_PORT  ephemeral port for phase 1 (default: PORT+1)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FRONTEND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$FRONTEND_DIR/.." && pwd)"

PORT="${PORT:-3000}"
DB="${E2E_DB:-e2e.db}"
API="${E2E_API:-$REPO_ROOT/target/release/buywise-api}"
# Phase-1 migration port: deliberately NOT PORT, so Playwright (probing PORT)
# never sees the transient migrate-only listener.
MIGRATE_PORT="${E2E_MIGRATE_PORT:-$((PORT + 1))}"

# The server resolves `frontend/build` and a relative sqlite path against its
# CWD, so run from the repo root.
cd "$REPO_ROOT"

if [[ ! -x "$API" ]]; then
  echo "run-server.sh: buywise-api not found/executable at $API" >&2
  echo "               build it with: cargo build --release -p buywise-api" >&2
  exit 1
fi
if [[ ! -d "$REPO_ROOT/frontend/build" ]]; then
  echo "run-server.sh: frontend/build is missing — run 'npm run build' in frontend/ first" >&2
  exit 1
fi

# 1. Fresh DB every run.
rm -f "$DB" "$DB-shm" "$DB-wal"

# 2. PHASE 1: throwaway server on MIGRATE_PORT to create + migrate the schema.
DATABASE_URL="sqlite://$DB" PORT="$MIGRATE_PORT" RUST_LOG="${RUST_LOG:-warn}" "$API" &
MIGRATE_PID=$!
migrate_cleanup() { kill "$MIGRATE_PID" 2>/dev/null || true; }
trap migrate_cleanup EXIT INT TERM

# Wait until it is listening on MIGRATE_PORT — that only happens after migrations
# have run (buywise-api migrates BEFORE it binds), so a 200 here means the schema
# exists on disk.
migrated=""
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "http://localhost:$MIGRATE_PORT/" 2>/dev/null; then
    migrated=1
    break
  fi
  sleep 0.5
done
if [[ -z "$migrated" ]]; then
  echo "run-server.sh: migration server did not come up on port $MIGRATE_PORT" >&2
  exit 1
fi

# Stop the throwaway server and wait for it to fully release the file/port before
# seeding, so the seed writes happen with NO server holding the DB.
kill "$MIGRATE_PID" 2>/dev/null || true
wait "$MIGRATE_PID" 2>/dev/null || true
trap - EXIT INT TERM

# 3. Seed the migrated file while no server is running (deterministic, no WAL race).
E2E_DB="$DB" bash "$SCRIPT_DIR/seed.sh"

# 4. PHASE 2: start the REAL server on PORT. Now `/` answering 200 on PORT is a
# true ready signal: the DB is already migrated AND seeded.
DATABASE_URL="sqlite://$DB" PORT="$PORT" RUST_LOG="${RUST_LOG:-warn}" "$API" &
SERVER_PID=$!
cleanup() { kill "$SERVER_PID" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# Keep the server in the foreground for Playwright's lifecycle management.
wait "$SERVER_PID"
