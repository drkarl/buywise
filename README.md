# BuyWise

A self-hosted household grocery list and savings app for Ireland. It compares prices across Tesco, Dunnes Stores, SuperValu, Aldi, and Lidl, and recommends the cheapest single-store or multi-store plan for the week's shopping list.

## Development status

**Working locally, not deployed.** The Rust workspace and the SvelteKit PWA build, lint, and pass their test suites, and the app runs end to end on a developer machine: sign-in, shared lists, offline sync, optimisation, purchases, receipts, and the savings ledger all work against a local SQLite database.

**Not proven:**

- It has never been deployed to a server and has no releases. The deployment runbook in [`deploy/README.md`](deploy/README.md) is written but untested on a real host.
- No one uses it beyond its author, so the real-world savings case is unvalidated.
- Live price ingest is untested against a live supplier: the BasketWatch adapter needs an API key and the Lidl adapter needs a feed URL, and neither has been run against a current, verified source. Price comparison therefore works only with imported or fixture offers plus household `product_matches` rows.
- The savings figures are estimates until purchases are reconciled with plans (tracked in [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md)).

**Optional, off by default:** an external LLM fallback for unmatched list terms (`LLM_FALLBACK_ENABLED`) and a local embedding shortlist for imported-line suggestions (`EMBEDDING_ENABLED`). Both are documented in [`docs/data-flows.md`](docs/data-flows.md); the app runs fully without either.

## What it does

- Collaborative shopping list with real-time sync across devices
- Bilingual entry (English and Spanish resolve to the same item)
- Four optimisation plans with plain-language reasons for each recommendation, once retailer offers and product matches exist
- Aldi side comparison of potential savings, split between packaged and fresh items
- Per-item product rules, preferred sizes, substitutions, and suggested retailer product matches
- Purchase-frequency suggestions based on check-off history; each must be confirmed before it is added to a list
- Offline-first PWA: the shopping list works without connectivity in-store, with sync status and pending items shown until the server confirms them
- Purchase ledger: hand-entered purchases, receipt photos (30-day retention), and structured imports, kept as a record separate from the list
- Savings ledger: separates opportunity estimates and total-only basket variances from verified savings
- Household data controls: JSON export, household deletion, and a documented inventory of what leaves the server
- Price ingest adapters for BasketWatch (RapidAPI) and Lidl, run manually or by an optional weekly timer

## Stack

| Layer | Choice |
|---|---|
| Backend | Rust + Axum |
| Frontend | SvelteKit (PWA, adapter-static) |
| Database | SQLite WAL + sqlx |
| Backup (intended) | Litestream → Cloudflare R2 |
| Hosting (intended) | Hetzner CAX11 ARM (~€5/month); see [`deploy/README.md`](deploy/README.md) |
| Real-time | Server-Sent Events |
| Offline storage | Dexie (IndexedDB) |

## Repository layout

```
Cargo.toml         Rust workspace manifest
crates/
  core/            Domain model, optimiser, matching, normalisation (shared heart)
  api/             Axum server — depends on core
  ingest/          BasketWatch + Lidl adapters — depends on core
  cli/             Operator CLI (bootstrap, passwords, ingest, imports, export, deletion)
frontend/          SvelteKit PWA
migrations/        SQLite migrations
fixtures/          Fixture data for tests and offline ingest runs
deploy/
  systemd/         Service unit files
  litestream.yml   Litestream → Cloudflare R2 config
docs/
  design.md        Architecture and design document
  data-flows.md    Data-flow inventory, external processing, retention, deletion and backup review
  household-export.md  The household export JSON layout
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

The app is now at **http://localhost:3000**. Migrations seed the default
catalogue household, product catalogue, categories, and per-store aisle ordering,
but no users. Bootstrap creates a household with its first owner before you can
log in.

### 3. Create the first household and owner

Run the one-time bootstrap command from the repository root. It creates and
migrates the database if needed, then prompts for a password without echoing it:

```bash
cargo run -p buywise-cli -- bootstrap-household --db buywise.db \
  --name "Our Household" --email owner@example.com
```

The first account is the household owner. The owner adds other members in
**Settings → Household members** and can share each member's initial password
out of band. See [`deploy/README.md`](deploy/README.md) for deployed setup.

### 4. Log in and open the app

Open **http://localhost:3000**. The home page is the auth gate and list index:
sign in with the email and password you chose during bootstrap. Create a list or
open an existing one; you can also reach the savings ledger from here.

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
| `/purchases` | Household purchase history; add, edit, link receipt photos, and delete hand-entered purchases |
| `/settings` | Store status, nearby settings, category rules, and convenience thresholds |
| `/savings` | Savings ledger |

To verify the API directly from the shell instead of the browser, sign in with the
email and password you chose in the bootstrap step above (migrations seed no
users, so a login before bootstrap returns `401`):

```bash
# Log in, saving cookies (session + csrf):
curl -c /tmp/bw.cookies -X POST http://localhost:3000/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"owner@example.com","password":"<the password you chose>"}'

# List the household's lists, then fetch one by its id (session-authenticated):
curl -b /tmp/bw.cookies http://localhost:3000/api/lists
curl -b /tmp/bw.cookies http://localhost:3000/api/list/<list-id>
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

## Household data controls

What leaves the device and the server, who can read it and how long it is kept is inventoried in
[`docs/data-flows.md`](docs/data-flows.md) — including the optional external LLM fallback
(`LLM_FALLBACK_ENABLED`, off by default), the optional local embedding shortlist
(`EMBEDDING_ENABLED`, off by default, no data leaves the server), the Litestream → Cloudflare R2
backup review, and the deletion and raw-receipt retention policy. The JSON layout produced by export
is documented in [`docs/household-export.md`](docs/household-export.md).

Household members can download the export from Settings → **Your data**. Only
owners can delete household data or manage members there; the CLI commands below
remain available to operators.

```bash
# Export a household's records as one JSON document (read-only; omit --out for stdout).
cargo run -p buywise-cli -- export-household --db buywise.db --household-id <uuid> --out household.json

# See what deletion would remove, then delete it (--confirm is required to delete).
cargo run -p buywise-cli -- delete-household --db buywise.db --household-id <uuid> --dry-run
cargo run -p buywise-cli -- delete-household --db buywise.db --household-id <uuid> --confirm

# Receipt photos are kept 30 days; the daily timer runs this sweep, and it can be run by hand.
cargo run -p buywise-cli -- expire-receipts --db buywise.db
```

Receipt photos live on the server filesystem (not in SQLite and not in the database backup) and are
served only to the household through the app's **Receipts** page, which uploads, lists, views and
deletes them; `docs/data-flows.md` §4 has the storage layout, limits and retention rules.

Deletion clears the household from the live database immediately; it does not rewrite backups, and
a restore can resurrect it — `docs/data-flows.md` §5 explains the expiry and restore handling.

## Known limitations

See [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md) for accepted gaps and the follow-up that removes each.

## Build phases

All six phases are implemented locally and covered by the test suites; none has
been exercised against a real deployment or live price feed.

| Phase | Scope |
|---|---|
| 0 | CLI optimiser with fixture data — validates the core logic end-to-end |
| 1 | Axum API: auth, list CRUD, mutation endpoint, SSE stream |
| 2 | SvelteKit PWA: list UI, offline sync, shopping mode |
| 3 | Bilingual resolution cascade + optional LLM fallback |
| 4 | Optimiser UI: four plans, plain-language recommendation |
| 5 | Household learning, match feedback, savings ledger |

## License

BuyWise is dual-licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
