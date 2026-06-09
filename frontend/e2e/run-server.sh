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
# Order of operations (Playwright's globalSetup runs BEFORE webServer, so we
# cannot seed there against a not-yet-migrated DB — instead this wrapper IS the
# webServer command and seeds itself):
#   1. delete any stale e2e DB so every run starts from clean, known state;
#   2. start the server in the background — it runs migrations on boot, creating
#      the schema in the fresh file;
#   3. wait for the health endpoint, then run seed.sh against the live DB
#      (SQLite WAL tolerates the concurrent seed writes);
#   4. hand the server process to the foreground (exec-like wait) so Playwright
#      manages its lifecycle and tears it down after the run.
#
# Env:
#   PORT        port to serve on                 (default: 3000; webServer matches)
#   E2E_DB      SQLite file path                 (default: e2e.db, gitignored)
#   E2E_API     path to the buywise-api binary   (default: release build)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FRONTEND_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$FRONTEND_DIR/.." && pwd)"

PORT="${PORT:-3000}"
DB="${E2E_DB:-e2e.db}"
API="${E2E_API:-$REPO_ROOT/target/release/buywise-api}"

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

# 2. Start the server (migrates on boot).
DATABASE_URL="sqlite://$DB" PORT="$PORT" RUST_LOG="${RUST_LOG:-warn}" "$API" &
SERVER_PID=$!

# Forward termination to the child so Playwright's SIGTERM cleanly stops it.
cleanup() { kill "$SERVER_PID" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

# 3. Wait for readiness (any HTTP response from the API means it is listening;
# /api/auth/me returns 401 unauthenticated, which is "up").
ready=""
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null "http://localhost:$PORT/api/auth/me" 2>/dev/null \
     || [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/api/auth/me" 2>/dev/null)" == "401" ]]; then
    ready=1
    break
  fi
  sleep 0.5
done
if [[ -z "$ready" ]]; then
  echo "run-server.sh: server did not become ready on port $PORT" >&2
  exit 1
fi

E2E_DB="$DB" bash "$SCRIPT_DIR/seed.sh"

# 4. Keep the server in the foreground for Playwright's lifecycle management.
wait "$SERVER_PID"
