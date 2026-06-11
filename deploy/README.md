# BuyWise — Hetzner deployment

End-to-end runbook for deploying BuyWise to a Hetzner CAX11 (ARM64 / `aarch64`)
Ubuntu server. The single `buywise-api` binary serves both the SvelteKit PWA
static files and the Axum API on one port. A Litestream sidecar continuously
replicates the SQLite database to Cloudflare R2.

## Architecture

Two long-running systemd services run side by side, plus a weekly ingest timer:

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
| `PORT` | no | `3000` | TCP port the API binds on (`0.0.0.0:PORT`). |
| `RUST_LOG` | no | `info` | `tracing-subscriber` `EnvFilter` log directive. |

**`litestream` (Cloudflare R2 credentials):**

| Variable | Required | Purpose |
| --- | --- | --- |
| `LITESTREAM_ACCESS_KEY_ID` | yes | R2 API token access key id. |
| `LITESTREAM_SECRET_ACCESS_KEY` | yes | R2 API token secret. |

The variable names are referenced by `deploy/litestream.yml`. The default values
above are the in-code defaults read by `crates/api/src/main.rs`.

**Weekly ingest (`buywise-ingest.service`):**

| Variable | Required | Purpose |
| --- | --- | --- |
| `LIDL_FEED_URL` | no | Source URL for the Lidl direct ingest adapter. If unset, the Lidl half of the weekly ingest is best-effort and is skipped without failing the timer; BasketWatch always runs. |

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
- enables and starts `buywise-ingest.timer` (the weekly ingest timer only — the
  long-running `buywise`/`litestream` units are left for you to enable once the
  binaries are in place);
- prints the next-step commands.

## 4. Set environment variables and the R2 endpoint

Edit `/etc/buywise/buywise.env` and fill in the values (see the table above).
Then edit `/etc/litestream.yml` and replace `<ACCOUNT_ID>` in the endpoint with
your Cloudflare account id (Cloudflare dashboard → R2). The bucket
(`buywise-backup`) must already exist in R2.

## 5. Enable and start the services

```bash
sudo systemctl enable --now buywise litestream
```

`setup.sh` already enabled `buywise-ingest.timer`; you do not enable it here.

## Weekly price ingest

Price data is refreshed weekly by a oneshot systemd unit driven by a timer:

- `buywise-ingest.service` (`Type=oneshot`) runs the ingest adapters as the
  `buywise` user with `/etc/buywise/buywise.env` loaded (so it picks up
  `LIDL_FEED_URL`). It runs, in order:
  - `buywise-cli ingest-basketwatch --db /var/lib/buywise/buywise.db` — the
    **primary** source. If it fails, the unit fails (visible in `systemctl
    status` / the journal).
  - `buywise-cli ingest-lidl --db /var/lib/buywise/buywise.db` — **best-effort**
    (its `ExecStart` is prefixed with `-`). If `LIDL_FEED_URL` is unset or the
    feed is broken, the Lidl step errors but does not fail the unit, so a Lidl
    problem never reds the timer or masks a successful BasketWatch run. The
    error is still logged to the journal.
- `buywise-ingest.timer` fires **weekly, Monday 03:00** local time (a
  low-traffic overnight window). It is `Persistent=true`, so if the host was
  down at the scheduled time, the run fires once on the next boot instead of
  waiting a full week.

> The ingest writes to the same `/var/lib/buywise/buywise.db` the API reads.
> SQLite WAL allows a single writer alongside concurrent readers and Litestream
> replicates the WAL, so the weekly batch writer is safe beside the running API.

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

## 6. Smoke test (matches acceptance criteria)

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
This is order-sensitive: `buywise-api` opens the DB with `create_if_missing`, so
if it starts (or is enabled at boot and the box reboots) before the restore
completes, it will create an empty DB and race the restore. The restore must run
into an empty path, so `buywise` must be **stopped and disabled** first.

```bash
# 1. Stop AND disable buywise so it cannot start (now or at the next boot)
#    and race the restore by re-creating an empty DB. litestream is also down.
sudo systemctl disable --now buywise litestream

# 2. Make sure the target path is empty so restore writes a fresh copy.
sudo rm -f /var/lib/buywise/buywise.db /var/lib/buywise/buywise.db-wal /var/lib/buywise/buywise.db-shm

# 3. Restore the latest replica from R2 into the live path. Run as root with the
#    env file loaded: the R2 credentials live only in the 0600 root-owned env
#    file, which the buywise user cannot read.
sudo bash -c 'set -a; . /etc/buywise/buywise.env; set +a; litestream restore -config /etc/litestream.yml /var/lib/buywise/buywise.db'

# 4. The restore ran as root, so hand the DB back to the buywise user.
sudo chown buywise:buywise /var/lib/buywise/buywise.db

# 5. Re-enable and start the services, in order, only AFTER the restore is done.
sudo systemctl enable --now buywise litestream
```

> Do not run `litestream restore` while `buywise` is enabled or active — the
> `After=buywise.service`/`Wants=buywise.service` ordering on `litestream.service`
> governs normal startup, but it does not protect a manual restore from an
> already-running app. Disabling buywise (step 1) is what guards the restore.
