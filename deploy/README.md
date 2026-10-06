# BuyWise — Hetzner deployment

End-to-end runbook for deploying BuyWise to a Hetzner CAX11 (ARM64 / `aarch64`)
Ubuntu server. The single `buywise-api` binary serves both the SvelteKit PWA
static files and the Axum API on one port. A Litestream sidecar continuously
replicates the SQLite database to Cloudflare R2.

## Architecture

Two long-running systemd services run side by side, plus a weekly ingest timer
(and, once you serve the app to phones, the optional Caddy reverse proxy from
section 7):

- `buywise.service` — runs `/usr/local/bin/buywise-api` as the `buywise` user.
- `litestream.service` — runs `/usr/local/bin/litestream replicate` against the
  same SQLite database, replicating to Cloudflare R2.
- `buywise-ingest.timer` → `buywise-ingest.service` — a weekly oneshot that runs
  the price-ingest adapters (`buywise-cli ingest-basketwatch` + `ingest-lidl`)
  to refresh offer data. See "Weekly price ingest" below.

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

All variables live in `/etc/buywise/buywise.env`, loaded by both services.

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

**Weekly ingest (`buywise-ingest.service`):**

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `BASKETWATCH_API_KEY` | **yes** | — | BasketWatch API key. The `ingest-basketwatch` run is the primary, non-best-effort source: without this key it fails (`MissingApiKey`) and reds the whole weekly unit. Set it before the first weekly fire. |
| `BASKETWATCH_API_HOST` | no | `basketwatch.p.rapidapi.com` | BasketWatch API host. Only set to override the default RapidAPI host. |
| `LIDL_FEED_URL` | no | — | Source URL for the Lidl direct ingest adapter. If unset, the Lidl half of the weekly ingest is best-effort and is skipped without failing the timer; BasketWatch always runs. |

**LLM resolution fallback (`buywise-api`; optional, default off):**

The API can fall back to a cheap LLM call to resolve a list term that matches no
catalogue item (`no_match`). It is **off by default** and the API runs fine
without any of these set — an unresolved term simply stays `unresolved`. When
`LLM_FALLBACK_ENABLED=true`, both `LLM_API_KEY` and `LLM_MODEL` are required for
a request to be made; if either is missing the fallback silently stays disabled
(no startup error). Every provider is assumed OpenAI-chat-completions-compatible.

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
`tower-http`'s `ServeDir`. Until Phase 2 scaffolds the frontend, `GET /` returns
404 and only the `/api/*` routes are live — that is expected and the API still
deploys and runs. Once the frontend exists, build it (`npm run build` in
`frontend/`) and ship `frontend/build/` to the server's working directory
(`/var/lib/buywise/frontend/build`).

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
- installs the systemd units to `/etc/systemd/system/` (including
  `buywise-ingest.service` and `buywise-ingest.timer`);
- installs `litestream.yml` to `/etc/litestream.yml`;
- runs `systemctl daemon-reload`;
- *enables* (but does not start) `buywise-ingest.timer` — the weekly ingest
  timer only; it is started in step 5 after the env file is set, because its
  `Persistent=true` would otherwise fire a catch-up run immediately (the Monday
  03:00 point has already elapsed) and red the ingest with `MissingApiKey`
  before you configure `BASKETWATCH_API_KEY`. The long-running `buywise`/
  `litestream` units are likewise left for you to enable once the binaries are
  in place;
- installs the Caddy HTTPS reverse proxy, enables `caddy.service`, and restarts
  it to load the config **only** once you have set the hostname in
  `deploy/Caddyfile` and installed Caddy — an opt-in step that never enables the
  proxy with the placeholder still in place (see section 7);
- prints the next-step commands.

## 4. Set environment variables and the R2 endpoint

Edit `/etc/buywise/buywise.env` and fill in the values (see the table above).
Then edit `/etc/litestream.yml` and replace `<ACCOUNT_ID>` in the endpoint with
your Cloudflare account id (Cloudflare dashboard → R2). The bucket
(`buywise-backup`) must already exist in R2.

## 5. Enable and start the services

Do this only **after** the env file (step 4) is set — the weekly ingest needs
`BASKETWATCH_API_KEY` before it first runs.

```bash
sudo systemctl enable --now buywise litestream

# `setup.sh` already *enabled* buywise-ingest.timer (persisted for the next
# boot) but deliberately did not start it. Start it now that the env file holds
# BASKETWATCH_API_KEY, so a Persistent=true catch-up run can't red the ingest
# with MissingApiKey before configuration:
sudo systemctl start buywise-ingest.timer
```

## 6. Create the first household and owner (one-time)

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

## 7. Smoke test (matches acceptance criteria)

> These checks exercise the loopback HTTP listener directly. To serve the app to
> phones over `https://<hostname>`, complete section 7 (HTTPS reverse proxy) and
> run its verification block.

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

## Weekly price ingest

Price data is refreshed weekly by a oneshot systemd unit driven by a timer:

- `buywise-ingest.service` (`Type=oneshot`) runs the ingest adapters as the
  `buywise` user with `/etc/buywise/buywise.env` loaded (so it picks up
  `BASKETWATCH_API_KEY` and `LIDL_FEED_URL`). It runs, in order:
  - `buywise-cli ingest-basketwatch --db /var/lib/buywise/buywise.db` — the
    **primary** source. It **requires `BASKETWATCH_API_KEY`** (see the env
    table above); without it the run fails. If it fails, the unit fails (visible
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

## Logs and troubleshooting

```bash
journalctl -u buywise -f
journalctl -u litestream -f
journalctl -u buywise-ingest -f
```

- `buywise-api` logs at `RUST_LOG` level to the journal (`StandardOutput=journal`).
- A red weekly ingest means `ingest-basketwatch` failed (Lidl is best-effort and
  cannot fail the unit). Check `journalctl -u buywise-ingest` and confirm
  `/usr/local/bin/buywise-cli` exists and the DB path is writable by `buywise`.
- If `buywise` fails to start, check `DATABASE_URL` points at a writable path
  owned by the `buywise` user.
- If `litestream` fails, check the R2 credentials in `buywise.env` and the
  `endpoint`/`region` in `/etc/litestream.yml`.

## Disaster recovery (restore from R2)

If the server is lost, restore the database from R2 **before** `buywise` runs.
This is order-sensitive: both `buywise-api` and the weekly ingest (`buywise-cli`)
open the DB with `create_if_missing`, so if either starts before the restore
completes it will create an empty DB and race the restore. The restore must run
into an empty path, so `buywise` **and** the ingest timer must be **stopped and
disabled** first. The timer matters even when no run is due: `Persistent=true`
fires a missed run on the next boot, which can land mid-restore.

```bash
# 1. Stop AND disable every writer so none can start (now or at the next boot)
#    and race the restore by re-creating an empty DB. This covers buywise,
#    litestream, and the weekly ingest timer; the explicit `stop` of the ingest
#    service also kills a run that may already be in progress.
sudo systemctl disable --now buywise litestream buywise-ingest.timer
sudo systemctl stop buywise-ingest.service

# 2. Make sure the target path is empty so restore writes a fresh copy.
sudo rm -f /var/lib/buywise/buywise.db /var/lib/buywise/buywise.db-wal /var/lib/buywise/buywise.db-shm

# 3. Restore the latest replica from R2 into the live path. Run as root with the
#    env file loaded: the R2 credentials live only in the 0600 root-owned env
#    file, which the buywise user cannot read.
sudo bash -c 'set -a; . /etc/buywise/buywise.env; set +a; litestream restore -config /etc/litestream.yml /var/lib/buywise/buywise.db'

# 4. The restore ran as root, so hand the DB back to the buywise user.
sudo chown buywise:buywise /var/lib/buywise/buywise.db

# 5. Re-enable and start the services, in order, only AFTER the restore is done.
#    Re-enable the ingest *timer* (not the oneshot service); it resumes its
#    weekly schedule.
sudo systemctl enable --now buywise litestream buywise-ingest.timer
```

> Do not run `litestream restore` while `buywise` or the ingest timer is enabled
> or active — the `After=buywise.service`/`Wants=buywise.service` ordering on
> `litestream.service` governs normal startup, but it does not protect a manual
> restore from an already-running app, and nothing orders the weekly ingest
> against a restore. Disabling buywise and `buywise-ingest.timer` (step 1) is
> what guards the restore.
