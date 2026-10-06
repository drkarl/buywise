# BuyWise — Hetzner deployment

End-to-end runbook for deploying BuyWise to a Hetzner CAX11 (ARM64 / `aarch64`)
Ubuntu server. The single `buywise-api` binary serves both the SvelteKit PWA
static files and the Axum API on one port. A Litestream sidecar continuously
replicates the SQLite database to Cloudflare R2.

## Architecture

Two long-running systemd services run side by side, plus a daily receipt-expiry
timer (installed and enabled by `setup.sh`), an optional weekly ingest timer
(disabled by default) and, once you serve the app to phones, the optional Caddy
reverse proxy from section 7:

- `buywise.service` — runs `/usr/local/bin/buywise-api` as the `buywise` user.
- `litestream.service` — runs `/usr/local/bin/litestream replicate` against the
  same SQLite database, replicating to Cloudflare R2.
- `buywise-ingest.timer` → `buywise-ingest.service` — **optional** weekly oneshot
  that runs the price-ingest adapters (`buywise-cli ingest-basketwatch` +
  `ingest-lidl`) to refresh offer data. `setup.sh` installs it **without enabling
  it**; the first deploy runs without price ingest. See "Weekly price ingest"
  below for when and how to turn it on.
- `buywise-expire-receipts.timer` → `buywise-expire-receipts.service` — daily
  oneshot that deletes receipt image files and `receipt_images` rows once their
  30-day expiry has passed. `setup.sh` installs **and enables** it, so the
  retention policy is enforced from the first deploy with no operator action.
  See "Daily receipt-expiry sweep" below.

| Item | Value |
| --- | --- |
| API binary | `/usr/local/bin/buywise-api` (package `crates/api`) |
| CLI binary | `/usr/local/bin/buywise-cli` (package `crates/cli`) |
| Litestream binary | `/usr/local/bin/litestream` |
| Database | `/var/lib/buywise/buywise.db` |
| Env file | `/etc/buywise/buywise.env` |
| Litestream config | `/etc/litestream.yml` |
| Service user | `buywise` (no-login system user) |

## Environment variables

All variables live in `/etc/buywise/buywise.env`, loaded by the services.

**`buywise-api` (the application):**

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `DATABASE_URL` | recommended (to make the path explicit) | `sqlite://buywise.db` | SQLite connection URL. The default resolves under `WorkingDirectory=/var/lib/buywise` to the same file, but set it to `sqlite:///var/lib/buywise/buywise.db` to make the Litestream-replicated path explicit. |
| `PORT` | no | `3000` | TCP port the API binds on (`0.0.0.0:PORT`). If you change it, update the upstream port in `deploy/Caddyfile` to match. |
| `RUST_LOG` | no | `info` | `tracing-subscriber` `EnvFilter` log directive. |

**`litestream` (Cloudflare R2 credentials):**

| Variable | Required | Purpose |
| --- | --- | --- |
| `LITESTREAM_ACCESS_KEY_ID` | yes | R2 API token access key id. |
| `LITESTREAM_SECRET_ACCESS_KEY` | yes | R2 API token secret. |

The variable names are referenced by `deploy/litestream.yml`. The default values
above are the in-code defaults read by `crates/api/src/main.rs`.

**Weekly ingest (`buywise-ingest.service`; optional — installed but not enabled):**

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `BASKETWATCH_API_KEY` | **yes, to run it** | — | BasketWatch API key. The `ingest-basketwatch` run is the primary, non-best-effort source: without this key it fails (`MissingApiKey`) and reds the whole weekly unit. Set it before you enable the timer, not before the first deploy. |
| `BASKETWATCH_API_HOST` | no | `basketwatch.p.rapidapi.com` | BasketWatch API host. Only set to override the default RapidAPI host. |
| `LIDL_FEED_URL` | no | — | Source URL for the Lidl direct ingest adapter. If unset, the Lidl half of the weekly ingest is best-effort and is skipped without failing the timer; BasketWatch always runs. There is no default and no verified Lidl feed yet. |

Leave all three unset while the ingest stays disabled; see "Weekly price ingest"
below for when to enable it.

**Receipt expiry (`buywise-expire-receipts.service`; installed and enabled):**

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `RECEIPT_DIR` | no | `<directory of the DB>/receipts` (`/var/lib/buywise/receipts`) | Directory holding the receipt image files the daily sweep deletes. Set it only if you store receipt images somewhere other than the default next to the database. |

Leave it unset to use the default directory; see "Daily receipt-expiry sweep"
below.

**LLM resolution fallback (`buywise-api`; optional, default off):**

The API can fall back to a cheap LLM call to resolve a list term that matches no
catalogue item (`no_match`). It is **off by default** and the API runs fine
without any of these set — an unresolved term simply stays `unresolved`. When
`LLM_FALLBACK_ENABLED=true`, both `LLM_API_KEY` and `LLM_MODEL` are required for
a request to be made; if either is missing the fallback silently stays disabled
(no startup error). Every provider is assumed OpenAI-chat-completions-compatible.
If it is ever turned on, the raw list term and the household's canonical item
names (and, for candidate ranking, the item description and candidate product
labels) leave the server for the configured endpoint — the full inventory of
what does and does not leave is in
[`docs/data-flows.md`](../docs/data-flows.md) §2.6. Keep it off unless you have
chosen to accept that flow.

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `LLM_FALLBACK_ENABLED` | no | `false` | Master switch. `true` (case-insensitive) turns the fallback on; anything else (including unset) leaves it off. |
| `LLM_API_KEY` | when enabled | — | API key sent as a bearer token. Required for a request to be made when the fallback is enabled. |
| `LLM_MODEL` | when enabled | — | Model name. Never hardcoded (providers rotate cheap models often), so the operator picks one. Required when the fallback is enabled. |
| `LLM_PROVIDER` | no | — | Provider identifier. Currently accepted but unused at runtime (every provider is treated as OpenAI-compatible); reserved for future per-provider defaults. |
| `LLM_BASE_URL` | no | `https://api.openai.com/v1/chat/completions` | Chat-completions endpoint. Override for self-hosted / proxy / non-OpenAI deployments. |

> **The API runs without any ingest or LLM variables.** It starts and serves
> requests with none of the above set. But the **price comparison stays empty**
> until the database holds both retailer **offers** (from the weekly ingest, see
> "Weekly price ingest" below) **and** `product_matches` rows mapping canonical
> items to retailer products. The optimiser filters retailer candidates through
> `product_matches`; with no offers and no matches there is nothing to price. The
> initial matches can be seeded out-of-band via `buywise-cli seed-matches --from
> <csv> --db <db> --household-id <uuid>` (idempotent upsert) — see the Phase-0
> seeding flow in [`docs/phase0-results.md`](../docs/phase0-results.md). The app
> can also propose retailer products for unmatched items in the item's
> preferences screen; accepting a proposal creates a match. The LLM fallback can
> rank those proposals when enabled, in addition to its separate term-resolution
> use.

## 1. Build the ARM64 binary (on your build machine)

Install the cross-compilation target and a linker, then build:

```bash
# One-time: add the aarch64 Rust target.
rustup target add aarch64-unknown-linux-gnu

# One-time (Debian/Ubuntu build host): install the cross linker.
sudo apt-get install -y gcc-aarch64-linux-gnu

# Tell cargo which linker to use for the target (or put this in .cargo/config.toml).
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

cargo build --release --target aarch64-unknown-linux-gnu -p buywise-api
```

The binary lands at `target/aarch64-unknown-linux-gnu/release/buywise-api`.

> If cross-compilation is troublesome, build natively on the CAX11 itself
> (ARM): `cargo build --release -p buywise-api`.

### Frontend (SvelteKit PWA)

The API serves the SvelteKit static build from `frontend/build/` via
`tower-http`'s `ServeDir`. Build it on your build machine and ship it to the
server's working directory (`/var/lib/buywise/frontend/build`) **before** the
`buywise` service starts: without it, `GET /` and every non-`/api` path returns
404 (only the `/api/*` routes are live) and the PWA cannot be installed.

```bash
cd frontend
npm ci
npm run build        # emits the static SPA (PWA manifest + service worker) into frontend/build
```

Build output must include `200.html` (the SPA shell), `manifest.webmanifest` and
`sw.js`; `deploy/smoke.sh` checks the manifest and service worker are served (see
section 6). Copy the whole directory to the server in section 2.

## 2. Copy binaries to the server

Copy the API binary and the CLI binary (the CLI runs the weekly price ingest):

```bash
scp target/aarch64-unknown-linux-gnu/release/buywise-api root@<server>:/usr/local/bin/buywise-api
scp target/aarch64-unknown-linux-gnu/release/buywise-cli root@<server>:/usr/local/bin/buywise-cli
```

> Build the CLI the same way as the API, swapping the package:
> `cargo build --release --target aarch64-unknown-linux-gnu -p buywise-cli`
> (native on the CAX11: `cargo build --release -p buywise-cli`). The binary
> lands at `target/aarch64-unknown-linux-gnu/release/buywise-cli`.

Copy the built frontend (section 1) to the server's working directory. It must
land at `/var/lib/buywise/frontend/build`, and the `buywise` service user must be
able to read it:

```bash
tar -C frontend -czf - build \
  | ssh root@<server> 'mkdir -p /var/lib/buywise/frontend && \
      tar -C /var/lib/buywise/frontend -xzf - && \
      chown -R buywise:buywise /var/lib/buywise/frontend'
```

Re-run this copy after every frontend rebuild; the API serves whatever is on disk
at the moment of the request, so a new build needs no restart.

Install Litestream on the server (ARM64 release). On the server:

```bash
LITESTREAM_VERSION=v0.3.13
curl -L -o /tmp/litestream.tar.gz \
  "https://github.com/benbjohnson/litestream/releases/download/${LITESTREAM_VERSION}/litestream-${LITESTREAM_VERSION}-linux-arm64.tar.gz"
tar -C /usr/local/bin -xzf /tmp/litestream.tar.gz litestream
rm /tmp/litestream.tar.gz
```

Confirm all binaries are present and executable:

```bash
ls -l /usr/local/bin/buywise-api /usr/local/bin/buywise-cli /usr/local/bin/litestream
```

## 3. Run the setup script

Copy the `deploy/` directory to the server and run the setup script as root:

```bash
scp -r deploy root@<server>:/tmp/buywise-deploy
ssh root@<server>
cd /tmp/buywise-deploy
sudo ./setup.sh
```

`setup.sh` is idempotent. It:

- creates the `buywise` no-login system user (if missing);
- creates `/var/lib/buywise/` owned by `buywise`;
- creates `/etc/buywise/` and a `buywise.env` template (mode 600) if absent;
- installs the systemd units to `/etc/systemd/system/`, **including**
  `buywise-expire-receipts.service` and `buywise-expire-receipts.timer` (the
  daily receipt-expiry sweep) and the optional `buywise-ingest.service` and
  `buywise-ingest.timer`;
- installs `litestream.yml` to `/etc/litestream.yml`;
- runs `systemctl daemon-reload`;
- **installs and enables** `buywise-expire-receipts.timer`, so the 30-day
  receipt-expiry sweep runs from the first deploy without operator action (see
  "Daily receipt-expiry sweep"). On a host without a running systemd it prints a
  warning instead of failing; enable the timer by hand there;
- enables no long-running unit: `buywise`, `litestream` and the weekly ingest
  timer are all left for you to enable once the binaries and the env file are in
  place. The ingest timer in particular stays disabled until you turn it on on
  purpose — it is `Persistent=true`, so an enabled timer left unconfigured fires
  a catch-up run at the next boot and reds the ingest with `MissingApiKey`;
- installs the Caddy HTTPS reverse proxy, enables `caddy.service`, and restarts
  it to load the config **only** once you have set the hostname in
  `deploy/Caddyfile` and installed Caddy — an opt-in step that never enables the
  proxy with the placeholder still in place (see section 7);
- prints the next-step commands.

## 4. Set environment variables, and create the R2 bucket and token

### 4.1 BuyWise environment

Edit `/etc/buywise/buywise.env` and fill in the values (see the table above):

- `LITESTREAM_ACCESS_KEY_ID` / `LITESTREAM_SECRET_ACCESS_KEY` — the R2 token from
  §4.2. Backups cannot start without them.
- `DATABASE_URL` — set it to
  `sqlite:///var/lib/buywise/buywise.db` to make the Litestream-replicated path
  explicit.
- Leave the ingest variables (`BASKETWATCH_API_KEY`, `LIDL_FEED_URL`) unset for
  now: the first deploy runs without the price ingest (see "Weekly price
  ingest").
- Leave the LLM variables unset: the external AI fallback is off by default and
  stays off for the first deploy.

### 4.2 Cloudflare R2 bucket and scoped token

Backups replicate to a single R2 bucket reached with a token scoped to **only**
that bucket (the owner's decision, issue #136 — not an account-wide token).

1. **Create the bucket.** Cloudflare dashboard → **R2 object storage** →
   **Create bucket**, named `buywise-backup`. Leave the default location
   unless you need a jurisdiction (a jurisdictional bucket needs the matching
   endpoint, e.g. `https://<ACCOUNT_ID>.eu.r2.cloudflarestorage.com`).
2. **Create a scoped token.** R2 object storage → **Account Details** →
   **Manage** next to **API Tokens** → **Create Account API token** (a User API
   token works too; it is tied to your user). Then:
   - **Permissions:** `Object Read & Write` — the only thing Litestream needs
     (it reads, writes and lists objects; it never manages buckets). Do **not**
     pick `Admin Read & Write`: that would also let the token create and delete
     buckets.
   - **Scope:** with `Object Read & Write` selected, apply the token to
     **specific buckets only** and select `buywise-backup`. This is the only way
     to keep it off your other buckets.
3. **Copy the credentials** into `/etc/buywise/buywise.env`
   (`LITESTREAM_ACCESS_KEY_ID`, `LITESTREAM_SECRET_ACCESS_KEY`). The Secret
   Access Key is shown once — the dashboard will not show it again.
4. **Confirm the scope in the dashboard** before trusting it: open the token
   under **Manage API Tokens** and check it lists `buywise-backup` (not
   "All buckets") and shows a read-and-write object permission only. If it says
   all buckets, delete it and create a scoped one.
5. **Point Litestream at your account.** Edit `/etc/litestream.yml` and replace
   `<ACCOUNT_ID>` in the `endpoint` with your Cloudflare account id
   (dashboard → **R2**; the id is also in the endpoint URL Cloudflare shows for
   the bucket). The bucket must exist before `litestream` starts.

> R2 scopes a token to whole buckets, not prefixes, so this token can read and
> write every object under `buywise-backup`. That is the narrowest R2 offers and
> is what the owner chose; the runbook's restore rehearsal below checks it.

## 5. Enable and start the services

Do this only **after** the env file and R2 token (§4) are set.

```bash
sudo systemctl enable --now buywise litestream
```

Leave the weekly ingest timer **disabled** for now: the first deploy runs
without the price ingest, so do not enable or start `buywise-ingest.timer`.
"Weekly price ingest" below documents when and how to turn it on (after the
live-savings validation, issue #109), what it needs, and how to check it
produced offers.

The daily receipt-expiry timer needs no step here: `setup.sh` already installed
**and enabled** `buywise-expire-receipts.timer`, so the 30-day receipt retention
is enforced from the first deploy (see "Daily receipt-expiry sweep").

Then run the first-deploy steps in section 6.

## 6. First deploy: create the household and verify the deploy

### 6.1 Create the first household and owner (one-time)

The app has no self-registration, so the first household and its owner are
created with a one-time command. Run it once, before anyone signs in:

```bash
sudo -u buywise buywise-cli bootstrap-household \
  --db /var/lib/buywise/buywise.db \
  --name "Our Household" \
  --email owner@example.com
```

It creates and migrates the database file if it is missing, and prompts for the
owner's password (hidden; entered twice on a terminal). Keep that password and
sign in as that email; the owner then adds the rest of the household from
**Settings → Household members** (each new member gets an initial password the
owner shares out of band). The command refuses to run once the database has any
account, so it cannot be used to add a second household. See
[`docs/data-flows.md`](../docs/data-flows.md) §5.1.

> **First-deploy order.** Start the services (§5) → bootstrap the household and
> owner here → verify (§6.2) → optionally serve over HTTPS (§7). The weekly price
> ingest comes **later**: leave `buywise-ingest.timer` disabled until the
> live-savings validation (issue #109) has confirmed supplier access, terms and
> cost (see "Weekly price ingest"). The daily receipt-expiry timer needs nothing
> here — `setup.sh` already enabled it, so the 30-day retention runs from the
> first deploy (see "Daily receipt-expiry sweep"). The external AI fallback also
> stays off — its default — for the first deploy; if it is ever turned on, the
> raw list term and the household's canonical item names leave the server (see
> `docs/data-flows.md` §2.6).

### 6.2 Smoke test (matches acceptance criteria)

> These checks exercise the loopback HTTP listener directly. To serve the app to
> phones over `https://<hostname>`, complete section 7 (HTTPS reverse proxy) and
> run its verification block.

**One-command check:** `deploy/smoke.sh` runs everything below that can be
automated and exits non-zero with a clear message on the first failure. It checks
the health endpoint, the PWA manifest and service worker, and — when the
credentials below are set — an authenticated list read and a short SSE
connection. It never prints credentials.

```bash
# Anonymous checks only (health, manifest, service worker):
deploy/smoke.sh http://127.0.0.1:3000

# Or with an account, to also exercise auth and the live-sync stream:
BUYWISE_SMOKE_EMAIL=owner@example.com \
BUYWISE_SMOKE_PASSWORD='<the owner password>' \
  deploy/smoke.sh http://127.0.0.1:3000
```

Expected ending:

```
ok: API health reports ok
ok: PWA manifest served
ok: service worker served
ok: logged in
ok: authenticated list read succeeded
ok: SSE stream delivered a frame
SMOKE TEST PASSED
```

With no credentials it stops after the asset checks and prints `SMOKE TEST
PASSED` too. `deploy/smoke.sh --help` lists the environment variables (including
`BUYWISE_SMOKE_LIST_ID` to target a list and `BUYWISE_SMOKE_SSE_TIMEOUT`).

**Service is active:**

```bash
systemctl status buywise
systemctl status litestream
```

**API responds** (`/api/auth/me` returns a response — `401`/`200` depending on
session, not a connection refused):

```bash
curl -i http://localhost:3000/api/auth/me
```

**Litestream is replicating to R2:** the `litestream` CLI needs the R2
credentials, which live only in `/etc/buywise/buywise.env` (root-owned, `0600`)
and are otherwise injected solely by systemd's `EnvironmentFile`. Manual
`litestream` commands won't see them, so run them as root with that env file
loaded:

```bash
# Load the R2 credentials the same way systemd's EnvironmentFile does.
sudo bash -c 'set -a; . /etc/buywise/buywise.env; set +a; litestream replicas -config /etc/litestream.yml'
journalctl -u litestream -n 50 --no-pager
# Confirm objects appear under the buywise-backup bucket (R2 dashboard).
```

**Restart preserves the database** (WAL checkpointed, Litestream replicated):

```bash
# Note a row count or a known value before restart.
sudo systemctl restart buywise
# Confirm the local DB is intact after restart.
sqlite3 /var/lib/buywise/buywise.db 'PRAGMA integrity_check; SELECT count(*) FROM sqlite_master;'
curl -i http://localhost:3000/api/auth/me
```

**Replication reached R2** (restore the replica into a scratch path and compare
it to the live DB — proves the backup half of the acceptance criterion):

```bash
# Restore the latest R2 replica into a scratch file (does NOT touch the live DB).
# Run as root with the env file loaded so litestream sees the R2 credentials.
sudo bash -c 'set -a; . /etc/buywise/buywise.env; set +a; litestream restore -o /tmp/verify.db -config /etc/litestream.yml /var/lib/buywise/buywise.db'
# The restored copy must pass integrity_check and match the live row count.
sqlite3 /tmp/verify.db 'PRAGMA integrity_check; SELECT count(*) FROM sqlite_master;'
sqlite3 /var/lib/buywise/buywise.db 'SELECT count(*) FROM sqlite_master;'
sudo rm -f /tmp/verify.db  # written by root
```

> This proves replication on the spot. The formal, required restore rehearsal —
> with a place to record the timestamp, Litestream version, token scope check,
> retention and outcome — is in "Disaster recovery → Restore rehearsal".

## 7. HTTPS reverse proxy (Caddy)

Section 6 tests the raw listener on the loopback interface. A phone can only
install the PWA and use offline mode over HTTPS on a real hostname, because the
API sets its session cookie `Secure` and `SameSite=Strict`
(`crates/api/src/auth.rs`). Caddy is the supported TLS-terminating proxy: it
obtains and renews a certificate from Let's Encrypt automatically and forwards
every request to the API/SPA on `127.0.0.1:3000`. This is the one supported way
to serve BuyWise over HTTPS — do not put another proxy in front of it.

### 7.1 Point DNS at the server

Create these records at your DNS provider (choosing the hostname is out of scope
for this runbook):

| Type | Name | Value |
| --- | --- | --- |
| `A` | `<hostname>` | the server's public IPv4 address |
| `AAAA` | `<hostname>` | the server's public IPv6 address (if the server has one) |

Confirm they resolve to this host before continuing — Caddy asks Let's Encrypt
to validate the name, and certificate issuance fails until the records
propagate:

```bash
dig +short A <hostname>
dig +short AAAA <hostname>
```

### 7.2 Firewall: SSH and HTTPS only

Open SSH, HTTP and HTTPS, and nothing else. **Ports 80 and 443 must both be
reachable**: Caddy's automatic HTTPS uses port 80 for the ACME HTTP-01 challenge
and for the HTTP→HTTPS redirect, and port 443 for the TLS-ALPN-01 challenge and
for serving the app. The API port (`3000`) is deliberately **not** opened: the
API binds `0.0.0.0:3000` (a known limitation — see
[`docs/KNOWN_LIMITATIONS.md`](../docs/KNOWN_LIMITATIONS.md)), and only Caddy,
over loopback, reaches it.

```bash
sudo apt-get update
sudo apt-get install -y ufw
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw allow OpenSSH   # use `sudo ufw allow 22/tcp` if the profile is missing
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw enable
sudo ufw status verbose
```

`ufw status` must list `22`, `80` and `443` only (for both IPv4 and IPv6).
Nothing else — in particular no `3000` — is reachable from the internet.

### 7.3 Install Caddy (official repository, ARM64)

Caddy is installed from its official apt repository ([Caddy install docs]),
whose packages cover Ubuntu `arm64`. The package provides the `caddy.service`
unit that runs the proxy.

```bash
sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt-get update
sudo apt-get install -y caddy
caddy version
```

The package installs *and starts* `caddy.service` with a default Caddyfile. You
replace that file in 7.4.

[Caddy install docs]: https://caddyserver.com/docs/install#debian-ubuntu-raspbian

### 7.4 Set the hostname and install the Caddyfile

`deploy/Caddyfile` is the supported configuration, with the hostname left as a
placeholder. Replace **only** the `<hostname>` token on the site line — the rest
of the file already carries the SSE-safe proxy settings:

```bash
# From the deploy/ directory:
sed -i 's/<hostname>/buywise.example.com/' Caddyfile   # use the real hostname
```

Install it and restart Caddy by hand (or use the opt-in `setup.sh` step in 7.5):

```bash
sudo install -m 0644 Caddyfile /etc/caddy/Caddyfile
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl restart caddy
journalctl -u caddy -n 50 --no-pager   # watch the certificate being issued
```

### 7.5 The opt-in setup step

`sudo ./setup.sh` installs the Caddyfile to `/etc/caddy/Caddyfile`, enables
`caddy.service`, and restarts it to load the new config **only when** the site
line no longer has the `<hostname>` placeholder, the `caddy` binary is already
installed (7.3), and `deploy/Caddyfile` exists. With the placeholder on the site
line it prints guidance and leaves Caddy untouched, so a fresh host is never
brought up with an invalid hostname. Restarting also applies the new file when
the apt package has already started Caddy with its default config.

### 7.6 Proxy settings for the live-sync SSE stream

The live-sync endpoint `/api/sse/{list_id}` is a long-lived
`text/event-stream` connection. The settings that matter in `deploy/Caddyfile`
are:

- `flush_interval -1` puts the proxy in low-latency mode: response buffering is
  disabled and Caddy flushes immediately after each write to the client. Caddy
  already flushes a `text/event-stream` response immediately even without this
  setting, but stating it explicitly makes live sync independent of that
  content-type inference — each frame (a `mutation`, or the API's 30-second
  `heartbeat`) is written straight to the phone.
- `read_timeout 0` / `write_timeout 0` keep the proxy's own read/write timeouts
  disabled (0 = no timeout, Caddy's default), because the stream is idle between
  mutations and only carries the heartbeat. They are stated explicitly so a
  future edit does not lower them. The API emits that heartbeat precisely so
  intermediaries do not drop an idle connection (`crates/api/src/sse.rs`).

There is no `encode` directive, so Caddy does not compress the event stream.

### 7.7 Cookies behind the proxy

No proxy setting changes cookie behaviour, and none is needed:

- Caddy forwards the API's `Set-Cookie` headers unchanged and terminates TLS, so
  the browser sees `https://<hostname>` and keeps the `Secure` flag on both
  cookies.
- `SameSite=Strict` is satisfied because the SPA and the API share one origin
  (`https://<hostname>`): the login POST, the session cookie and the SSE
  connection are all same-site.
- Caddy sets `X-Forwarded-For`/`X-Forwarded-Proto` by default, but the API does
  not read them and does not alter cookie flags based on them.

Confirm the flags hold through the proxy in 7.8 step 3.

### 7.8 HTTPS verification

Run these once DNS resolves and Caddy is up (replace `<hostname>`).

**1. Certificate is live and the app is served.**

```bash
curl -sSI https://<hostname>/ | head -n 1
# expect: HTTP/2 200  (or HTTP/1.1 200)

echo | openssl s_client -connect <hostname>:443 -servername <hostname> 2>/dev/null \
  | openssl x509 -noout -subject -issuer -dates
# expect: subject=CN = <hostname>; issuer=... Let's Encrypt ...; notBefore/notAfter dates
```

**2. The API answers over HTTPS, and unauthenticated `/api/auth/me` is 401.**

```bash
curl -sS -o /dev/null -w '%{http_code}\n' https://<hostname>/
curl -sS -o /dev/null -w '%{http_code}\n' https://<hostname>/api/auth/me
# expect: 200 then 401
```

**3. Login cookie flags are intact behind the proxy.**

```bash
curl -sS -D - -o /dev/null -X POST https://<hostname>/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"<email>","password":"<password>"}' | grep -i '^set-cookie:'
# expect two Set-Cookie lines:
#   session=...; Path=/; HttpOnly; Secure; SameSite=Strict
#   csrf=...; Path=/; Secure; SameSite=Strict
```

**4. The SSE stream works through the proxy (i.e. it is not buffered).**

Use the `session` cookie from step 3 and a list id the logged-in household
owns. The connection must stay open and a frame must arrive **while it is
open** — an immediate `mutation`, or the 30-second `event: heartbeat`:

```bash
curl -sS -N --max-time 40 \
  -H 'Cookie: session=<session-token-from-step-3>' \
  'https://<hostname>/api/sse/<list-id>'
# expect: the connection stays open and prints `event: heartbeat` (and any
# `event: mutation` frames) before curl's 40s limit closes it. If nothing
# prints until the connection closes, something along the path is buffering:
# confirm flush_interval -1 is present in /etc/caddy/Caddyfile and that no
# other intermediary buffers the stream.
```

Once this passes, open `https://<hostname>/` on each phone and install the PWA.
Serving the app over HTTPS satisfies the PWA's secure-context requirement, so
offline mode and live sync work on both phones.

**5. Run the smoke script against the proxy.** `deploy/smoke.sh` re-checks all of
the above through Caddy, including that it does not buffer the SSE stream:

```bash
BUYWISE_SMOKE_EMAIL=owner@example.com \
BUYWISE_SMOKE_PASSWORD='<the owner password>' \
  deploy/smoke.sh https://<hostname>
```

It exits non-zero with a clear message on the first failure; the expected ending
is in section 6.2.

## Weekly price ingest (optional; the timer is disabled by default)

Price comparison needs retailer offers, refreshed weekly by a oneshot systemd
unit driven by a timer — but **the first deploy runs without it**. `setup.sh`
installs the units and deliberately leaves the timer disabled (section 3), and
the first-deploy order (section 6) keeps it that way. Enable it only after the
live-savings validation (issue #109) has confirmed supplier access, terms and
cost.

**What it needs before you enable it:**

- **An account and an API key.** `BASKETWATCH_API_KEY` is required: the
  BasketWatch run is the primary source and fails with `MissingApiKey` without
  it, which reds the whole unit. Set it in `/etc/buywise/buywise.env` (set
  `BASKETWATCH_API_HOST` only to use a different RapidAPI host).
- **Supplier terms checked against issue #109** — current access, coverage,
  refresh cadence, permitted private use and actual cost. The old $9.99/month
  RapidAPI figure is not current fact.
- **Which retailers it supplies.** The BasketWatch adapter requests Tesco,
  Dunnes, SuperValu and Aldi; it is not full coverage.
- **A Lidl feed, if you have one.** `LIDL_FEED_URL` has no default and there is
  no verified Lidl feed, so there is nothing to set until you have one whose
  terms you can rely on. The Lidl step is best-effort (its `ExecStart` is
  prefixed with `-`), so leaving it unset just skips Lidl and never fails the
  unit.
- **Household `product_matches`.** Ingested offers alone do not produce a price
  comparison: the optimiser prices only items that have `product_matches` rows
  (see the note under the environment tables). Seed them with
  `buywise-cli seed-matches` or accept proposals in the app.

**Enable it** (only once `BASKETWATCH_API_KEY` is set):

```bash
sudo systemctl enable --now buywise-ingest.timer
```

The `--now` after the scheduled Monday 03:00 point has elapsed fires a
`Persistent=true` catch-up run straight away — which is exactly why the key must
be in place first.

**Check the ingest produced offers** — a green unit only says the run exited 0:

```bash
sqlite3 /var/lib/buywise/buywise.db \
  "SELECT rp.source, count(*) FROM product_offers po \
     JOIN retailer_products rp ON rp.id = po.retailer_product_id GROUP BY rp.source;"
sqlite3 /var/lib/buywise/buywise.db \
  "SELECT source, status, finished_at FROM source_runs ORDER BY started_at DESC LIMIT 5;"
```

The first query must gain rows and the latest `source_runs` row must be `success`.
Empty offers or a `failed` run means it ran but is not delivering prices.

**How it runs:**

- `buywise-ingest.service` (`Type=oneshot`) runs the ingest adapters as the
  `buywise` user with `/etc/buywise/buywise.env` loaded (so it picks up
  `BASKETWATCH_API_KEY` and `LIDL_FEED_URL`). It runs, in order:
  - `buywise-cli ingest-basketwatch --db /var/lib/buywise/buywise.db` — the
    **primary** source. It **requires `BASKETWATCH_API_KEY`** (see the env
    table above); without it the run fails, and the unit fails with it (visible
    in `systemctl status` / the journal).
  - `buywise-cli ingest-lidl --db /var/lib/buywise/buywise.db` — **best-effort**
    (its `ExecStart` is prefixed with `-`). If `LIDL_FEED_URL` is unset or the
    feed is broken, the Lidl step errors but does not fail the unit, so a Lidl
    problem never reds the timer or masks a successful BasketWatch run. The
    error is still logged to the journal.
- `buywise-ingest.timer` fires **weekly, Monday 03:00** local time (a
  low-traffic overnight window). It is `Persistent=true`, so if the host was
  down at the scheduled time, the run fires once on the next boot instead of
  waiting a full week.

> The ingest writes to the same `/var/lib/buywise/buywise.db` the API uses.
> There are two potential writers — this weekly batch and `buywise-api` itself
> (sessions, lists, items, match feedback) — plus the API's reads. SQLite WAL
> serialises writers (one at a time) alongside concurrent readers, so the two
> can briefly contend during the batch (SQLite's busy-timeout governs the wait);
> Litestream replicates the WAL. The weekly batch writer is safe beside the
> running API, with at most brief lock waits.

> **Upgrading an existing deploy:** `setup.sh` only seeds these env vars into a
> *fresh* `/etc/buywise/buywise.env` (the template runs only when the file does
> not yet exist). On an existing host, manually append the new vars
> (`BASKETWATCH_API_KEY`, `BASKETWATCH_API_HOST`, `LIDL_FEED_URL`) to
> `/etc/buywise/buywise.env` — re-running `setup.sh` leaves the file untouched.

**Inspect the schedule and the last run:**

```bash
systemctl list-timers buywise-ingest.timer
systemctl status buywise-ingest.service
journalctl -u buywise-ingest -n 50 --no-pager
```

**Trigger a manual ingest now** (does not affect the weekly schedule):

```bash
sudo systemctl start buywise-ingest.service
journalctl -u buywise-ingest -f
```

## Daily receipt-expiry sweep (installed and enabled by default)

Receipt photos are kept for 30 days by policy (issue #149,
[`docs/data-flows.md`](../docs/data-flows.md) §4) and then deleted. `setup.sh`
installs **and enables** `buywise-expire-receipts.timer`, so from the first
deploy that retention is enforced automatically — there is nothing to configure
and no operator action to take. The sweep needs no network access and no
credentials beyond the local environment file.

**What it does.** `buywise-expire-receipts.service` (`Type=oneshot`) runs as the
`buywise` user with `/etc/buywise/buywise.env` loaded:

- `buywise-cli expire-receipts --db /var/lib/buywise/buywise.db` — deletes the
  receipt image files and their `receipt_images` rows whose 30-day expiry has
  passed, then prints a summary of the rows and files removed. It is safe to run
  repeatedly: a second run finds nothing left to remove and reports an empty
  summary.

The image directory defaults to `$RECEIPT_DIR` when that variable is set in
`/etc/buywise/buywise.env`, otherwise to `<directory of the DB>/receipts` (i.e.
`/var/lib/buywise/receipts`). Set `RECEIPT_DIR` only if you store receipt images
somewhere else. Like the ingest, the subcommand takes the DB as a filesystem
PATH, not the `sqlite://` `DATABASE_URL`.

**When it runs.** `buywise-expire-receipts.timer` fires **daily at 03:30** local
time (a low-traffic overnight window). A daily sweep is enough because expiry is
measured in whole days: anything that has expired is gone within a day, and a
daily run keeps each sweep small instead of letting a backlog build up. The
timer is `Persistent=true`, so a run missed while the host was down fires once on
the next boot instead of waiting another day.

**Verify expired rows and files are gone** (the run's summary says what it
removed; these confirm the result):

```bash
# Expired rows must be zero after a sweep. julianday() parses the ISO-8601
# `expires_at` text, so it compares correctly against now.
sqlite3 /var/lib/buywise/buywise.db \
  "SELECT count(*) FROM receipt_images WHERE julianday(expires_at) <= julianday('now');"
# Every surviving row should still have its file, and vice versa: these two
# counts should agree.
sqlite3 /var/lib/buywise/buywise.db "SELECT count(*) FROM receipt_images;"
ls -1 /var/lib/buywise/receipts | wc -l
```

**Inspect or trigger it:**

```bash
systemctl list-timers buywise-expire-receipts.timer
sudo systemctl start buywise-expire-receipts.service   # run a sweep now
journalctl -u buywise-expire-receipts -n 50 --no-pager
```

> The sweep writes to the same `/var/lib/buywise/buywise.db` the API uses.
> SQLite WAL serialises writers, so the delete briefly contends with the API at
> most; Litestream replicates the WAL. It is safe beside the running API.

## Logs and troubleshooting

```bash
journalctl -u buywise -f
journalctl -u litestream -f
journalctl -u buywise-ingest -f
journalctl -u buywise-expire-receipts -f
```

- `buywise-api` logs at `RUST_LOG` level to the journal (`StandardOutput=journal`).
- A red weekly ingest means `ingest-basketwatch` failed (Lidl is best-effort and
  cannot fail the unit). Check `journalctl -u buywise-ingest` and confirm
  `/usr/local/bin/buywise-cli` exists and the DB path is writable by `buywise`.
- A red receipt-expiry sweep means `expire-receipts` failed. Check
  `journalctl -u buywise-expire-receipts` and confirm `/usr/local/bin/buywise-cli`
  exists, the DB path is writable by `buywise`, and `RECEIPT_DIR` (if set) points
  at a directory the `buywise` user can read and write.
- If `buywise` fails to start, check `DATABASE_URL` points at a writable path
  owned by the `buywise` user.
- If `litestream` fails, check the R2 credentials in `buywise.env` and the
  `endpoint`/`region` in `/etc/litestream.yml`.

## Disaster recovery (restore from R2)

If the server is lost, restore the database from R2 **before** `buywise` runs.
This is order-sensitive: both `buywise-api` and the `buywise-cli` jobs (the
weekly ingest and the daily receipt-expiry sweep) open the DB with
`create_if_missing`, so if any of them starts before the restore completes it
will create an empty DB and race the restore. The restore must run into an empty
path, so `buywise` **and** both enabled timers must be **stopped and disabled**
first. Both timers matter even when no run is due: `Persistent=true` fires a
missed run on the next boot, which can land mid-restore.

```bash
# 1. Stop AND disable every writer so none can start (now or at the next boot)
#    and race the restore by re-creating an empty DB. This covers buywise,
#    litestream, the daily receipt-expiry timer (enabled by default) and the
#    weekly ingest timer; the explicit `stop` of the oneshot services also kills
#    a sweep/ingest that may already be in progress.
sudo systemctl disable --now buywise litestream buywise-expire-receipts.timer buywise-ingest.timer
sudo systemctl stop buywise-expire-receipts.service buywise-ingest.service

# 2. Make sure the target path is empty so restore writes a fresh copy.
sudo rm -f /var/lib/buywise/buywise.db /var/lib/buywise/buywise.db-wal /var/lib/buywise/buywise.db-shm

# 3. Restore the latest replica from R2 into the live path. Run as root with the
#    env file loaded: the R2 credentials live only in the 0600 root-owned env
#    file, which the buywise user cannot read.
sudo bash -c 'set -a; . /etc/buywise/buywise.env; set +a; litestream restore -config /etc/litestream.yml /var/lib/buywise/buywise.db'

# 4. The restore ran as root, so hand the DB back to the buywise user.
sudo chown buywise:buywise /var/lib/buywise/buywise.db

# 5. Re-enable and start the units that were enabled before, only AFTER the
#    restore is done: `buywise` and `litestream` always; the daily receipt-expiry
#    timer was enabled by setup.sh and should stay enabled; the weekly ingest
#    timer only if you had deliberately enabled it (it is disabled by default).
sudo systemctl enable --now buywise litestream
sudo systemctl enable --now buywise-expire-receipts.timer   # enabled by setup.sh
# sudo systemctl enable --now buywise-ingest.timer   # only if it was enabled before
```

> Do not run `litestream restore` while `buywise` or either timer is enabled
> or active — the `After=buywise.service`/`Wants=buywise.service` ordering on
> `litestream.service` governs normal startup, but it does not protect a manual
> restore from an already-running app, and nothing orders the CLI jobs against a
> restore. Disabling buywise and both timers (step 1) is what guards the restore.

### Restore rehearsal (required)

A documented disaster-recovery procedure that has never been rehearsed is not a
recovery plan. Run this **non-destructive** drill once before the deploy is
considered done, and again after any credential rotation or retention change. It
restores the replica into a scratch file and never touches the live database, so
it is safe while the services run.

```bash
# 1. Record what you are rehearsing with.
litestream version
grep -E 'retention|bucket|endpoint' /etc/litestream.yml

# 2. Restore the latest R2 replica into a scratch path (does NOT touch the live
#    DB). Run as root with the env file loaded so litestream sees the R2
#    credentials (see the note in section 6.2).
sudo bash -c 'set -a; . /etc/buywise/buywise.env; set +a; \
  litestream restore -o /tmp/rehearsal.db -config /etc/litestream.yml /var/lib/buywise/buywise.db'

# 3. The restored copy must open, pass integrity_check, and hold the live rows.
sqlite3 /tmp/rehearsal.db 'PRAGMA integrity_check; SELECT count(*) FROM sqlite_master;'
sqlite3 /var/lib/buywise/buywise.db 'SELECT count(*) FROM sqlite_master;'

# 4. Clean up (the file was written by root).
sudo rm -f /tmp/rehearsal.db
```

Step 1's `retention` output must be `168h` and the endpoint must be the R2
account endpoint. In the Cloudflare dashboard, confirm the token under **Manage
API Tokens** is scoped to `buywise-backup` only (see §4.2). If the token is
account-wide, stop and recreate it before recording the result.

Record the outcome (a copy of this table belongs in the deploy log/issue):

| Field | Value |
| --- | --- |
| Rehearsal timestamp (UTC) | |
| Litestream version | |
| Token scope check (bucket-scoped? dashboard confirmed) | |
| Retention in `/etc/litestream.yml` | |
| Outcome (integrity_check, row-count match, restore time) | |
| Operator | |

Tracking: this rehearsal, the scoped token and the explicit retention are the
backup-hardening items of [issue #136](https://github.com/drkarl/buywise/issues/136);
record the result there (or in the deploy issue) so it is not only on the server.
