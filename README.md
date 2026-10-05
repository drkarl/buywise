# BuyWise

Private household grocery optimiser for Ireland. Compares prices across Tesco, Dunnes Stores, SuperValu, Aldi, and Lidl — recommends the cheapest single-store or multi-store plan for the week's shopping list.

Built for two adults and two children. Not a public product.

## What it does

- Collaborative shopping list with real-time sync across devices
- Bilingual entry (English and Spanish resolve to the same item)
- Weekly price data via BasketWatch (RapidAPI) and Lidl direct
- Four optimisation plans with plain-language reasons for each recommendation
- Per-item product rules, preferred sizes, substitutions, and suggested retailer product matches
- Purchase suggestions based on check-off history; each must be confirmed before it is added to a list
- Offline-first PWA: the shopping list works without connectivity in-store, with sync status and pending items shown until the server confirms them
- Savings ledger: tracks what was actually saved over time

## Stack

| Layer | Choice |
|---|---|
| Backend | Rust + Axum |
| Frontend | SvelteKit (PWA, adapter-static) |
| Database | SQLite WAL + sqlx |
| Backup | Litestream → Cloudflare R2 |
| Hosting | Hetzner CAX11 ARM (~€5/month) |
| Real-time | Server-Sent Events |
| Offline storage | Dexie (IndexedDB) |

## Repository layout

```
Cargo.toml         Rust workspace manifest
crates/
  core/            Domain model, optimiser, matching, normalisation (shared heart)
  api/             Axum server — depends on core
  ingest/          BasketWatch + Lidl adapters — depends on core
  cli/             Validation CLI (Phase 0) — depends on core
frontend/          SvelteKit PWA
migrations/        SQLite migrations
fixtures/          Fixture data for CLI validation (Phase 0)
deploy/
  systemd/         Service unit files
  litestream.yml   Litestream → Cloudflare R2 config
docs/
  design.md        Architecture and design document
  KNOWN_LIMITATIONS.md  Accepted gaps and their follow-ups
```

## Running locally

BuyWise is a **single-origin app**: the Axum server serves both the JSON API under
`/api/*` and the compiled SvelteKit SPA (everything else) from `frontend/build`.
The frontend calls the API with same-origin relative paths (`/api/...`) and there
is **no Vite dev proxy**, so the way to run and try the app is: build the frontend
once, then run the server, which serves both on one port.

### Prerequisites

- Rust toolchain (`cargo`)
- Node.js ≥ 20 (`npm`)
- `sqlite3` CLI (used to seed the first user — see below)

### 1. Build the frontend

```bash
cd frontend
npm install
npm run build        # emits the static SPA into frontend/build
cd ..
```

### 2. Run the backend

```bash
cargo run -p buywise-api
```

On startup the server:

- opens (creating if missing) the SQLite database at `DATABASE_URL`
  (default `sqlite://buywise.db` in the repo root),
- runs all migrations in `migrations/`, and
- listens on `0.0.0.0:3000`.

| Env var | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | `sqlite://buywise.db` | SQLite database path |
| `PORT` | `3000` | TCP port |
| `RUST_LOG` | `info` | Log filter (`tracing` `EnvFilter`) |

The app is now at **http://localhost:3000**. The migrations seed the default
household, the product catalogue, categories, and per-store aisle ordering — but
**no users and no shopping lists** (those are created at runtime). Do the one-time
seed below before you can log in.

### 3. Seed the first user, membership, and a list

Users are deliberately **not** seeded by migrations (password hashes are never
committed). The `set-password` CLI only *updates* an existing user row, so you
first insert a user (+ household membership + a list) directly, then set its
password.

Start the server once first (step 2) so `buywise.db` exists and is migrated, then:

```bash
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
USER_ID="11111111-1111-1111-1111-111111111111"
HOUSEHOLD="00000000-0000-0000-0000-000000000001"   # the seeded default household
LIST_ID="22222222-2222-2222-2222-222222222222"

sqlite3 buywise.db <<SQL
INSERT OR IGNORE INTO users (id, email, password_hash, display_name, created_at)
  VALUES ('$USER_ID', 'me@buywise.local', 'placeholder', 'Me', '$NOW');
INSERT OR IGNORE INTO household_members (household_id, user_id, role, joined_at)
  VALUES ('$HOUSEHOLD', '$USER_ID', 'owner', '$NOW');
INSERT OR IGNORE INTO shopping_lists (id, household_id, name, status, current_revision, created_at)
  VALUES ('$LIST_ID', '$HOUSEHOLD', 'Weekly Shop', 'active', 0, '$NOW');
SQL

# Hash and store a real password for that user (argon2id):
printf 'buywise123\n' | cargo run -p buywise-cli -- set-password me@buywise.local --db buywise.db
```

> The household id **must** be `00000000-0000-0000-0000-000000000001` — that is the
> default household the catalogue is seeded against (see `migrations/004_seed_phase1.sql`).
> A user only sees data for households they belong to via `household_members`.

> `set-password` reads the password from stdin when it is not a TTY (shown above),
> or prompts twice (hidden) when run interactively. It errors if the email matches
> no existing user row.

### 4. Log in and open the app

Open **http://localhost:3000**. The home page is the auth gate and list index:
sign in with the email/password you set above (`me@buywise.local` / `buywise123`),
and your active lists appear as links. Pick one to open the shopping list; you can
also create further lists and reach the savings ledger from here.

The session cookie is `Secure`, which browsers accept over `http://localhost`
(treated as a secure context), so the cookie-based login works without HTTPS in
local dev.

Useful screens once logged in:

| Path | Screen |
|---|---|
| `/list/<id>` | Shopping list (add items, paste a list, real-time sync) |
| `/list/<id>/shopping` | In-store shopping mode |
| `/list/<id>/optimise` | The four optimisation plans |
| `/items/<canonical-item-id>/preferences?list=<id>` | Per-item product and substitution preferences, with known matches |
| `/list/<id>/trip` | Per-store lists for the chosen plan (plus unallocated items) |
| `/settings` | Household store preferences and convenience thresholds |
| `/savings` | Savings ledger |

To verify the API directly from the shell instead of the browser:

```bash
# Log in, saving cookies (session + csrf):
curl -c /tmp/bw.cookies -X POST http://localhost:3000/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"me@buywise.local","password":"buywise123"}'

# Fetch the list snapshot (session-authenticated):
curl -b /tmp/bw.cookies http://localhost:3000/api/list/22222222-2222-2222-2222-222222222222
```

> Mutating endpoints (`POST /api/mutations`, `POST /api/lists`, …) are CSRF-protected:
> echo the JS-readable `csrf` cookie value in an `X-CSRF-Token` header. The SPA does
> this automatically; raw `curl` calls to those endpoints must add the header manually.

### Frontend development (rebuild on change)

For live frontend work, rebuild on change and let the server keep serving `frontend/build`:

```bash
cd frontend && npm run build -- --watch
```

`npm run dev` (Vite dev server) runs the SPA on its own port but **cannot reach the
API** — there is no dev proxy, and all API calls are same-origin `/api/...`. Use the
build-and-serve flow above to exercise the full app.

## Design document

See [`docs/design.md`](docs/design.md) for the full architecture, schema, sync protocol, bilingual resolution cascade, and phased build plan.

## Known limitations

See [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md) for accepted gaps and the follow-up that removes each.

## Build phases

| Phase | Scope |
|---|---|
| 0 | CLI optimiser with fixture data — validates the core logic end-to-end |
| 1 | Axum API: auth, list CRUD, mutation endpoint, SSE stream |
| 2 | SvelteKit PWA: list UI, offline sync, shopping mode |
| 3 | Bilingual resolution cascade + LLM fallback |
| 4 | Optimiser UI: four plans, plain-language recommendation |
| 5 | Household learning, match feedback, savings ledger |
