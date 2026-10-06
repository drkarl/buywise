#!/usr/bin/env bash
#
# BuyWise deployment setup for a fresh Hetzner CAX11 (Ubuntu ARM) instance.
#
# Idempotent: safe to re-run. It creates the service user and data directory,
# installs the systemd units and the Litestream config, runs `daemon-reload`,
# and prints the env vars you must set in /etc/buywise/buywise.env before
# enabling the services. It does NOT enable or start the long-running services
# (buywise/litestream — the binaries must be in place first); it prints the
# exact commands to do so. It DOES `enable` (but deliberately does NOT `start`)
# the weekly ingest *timer*: enabling persists it for the next boot, while
# leaving the start to the post-env step prevents a Persistent=true catch-up
# run from firing before the operator has set BASKETWATCH_API_KEY. It also
# installs and enables the Caddy HTTPS reverse proxy, but ONLY once the operator
# has replaced the <hostname> placeholder in Caddyfile and Caddy is already
# installed (see deploy/README.md section 7); it never enables the proxy with the
# placeholder still in place.
#
# Prerequisites (see deploy/README.md): the buywise-api, buywise-cli, and
# litestream binaries must already be copied to /usr/local/bin/.
#
# Usage: sudo ./setup.sh
set -euo pipefail

# Resolve the directory this script lives in so the unit/config files are found
# regardless of the caller's working directory.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

readonly SERVICE_USER="buywise"
readonly DATA_DIR="/var/lib/buywise"
readonly ENV_DIR="/etc/buywise"
readonly ENV_FILE="${ENV_DIR}/buywise.env"
readonly SYSTEMD_DIR="/etc/systemd/system"
readonly LITESTREAM_CONFIG="/etc/litestream.yml"
readonly CADDY_CONFIG="/etc/caddy/Caddyfile"

if [[ "${EUID}" -ne 0 ]]; then
    echo "Error: this script must be run as root (use: sudo ./setup.sh)." >&2
    exit 1
fi

echo "==> Creating system user '${SERVICE_USER}' (if missing)"
if id -u "${SERVICE_USER}" >/dev/null 2>&1; then
    echo "    user '${SERVICE_USER}' already exists, skipping"
else
    # No-login system user: no home, no shell, no password.
    useradd --system --no-create-home --shell /usr/sbin/nologin "${SERVICE_USER}"
    echo "    created"
fi

echo "==> Creating data directory ${DATA_DIR}"
mkdir -p "${DATA_DIR}"
chown "${SERVICE_USER}:${SERVICE_USER}" "${DATA_DIR}"
chmod 750 "${DATA_DIR}"

echo "==> Creating env directory ${ENV_DIR}"
mkdir -p "${ENV_DIR}"

echo "==> Installing systemd units to ${SYSTEMD_DIR}"
install -m 0644 "${SCRIPT_DIR}/systemd/buywise.service" "${SYSTEMD_DIR}/buywise.service"
install -m 0644 "${SCRIPT_DIR}/systemd/litestream.service" "${SYSTEMD_DIR}/litestream.service"
install -m 0644 "${SCRIPT_DIR}/systemd/buywise-ingest.service" "${SYSTEMD_DIR}/buywise-ingest.service"
install -m 0644 "${SCRIPT_DIR}/systemd/buywise-ingest.timer" "${SYSTEMD_DIR}/buywise-ingest.timer"

echo "==> Installing Litestream config to ${LITESTREAM_CONFIG}"
# 0644 is intentional: litestream.yml contains no secrets (only placeholders);
# the R2 keys come from /etc/buywise/buywise.env (mode 600).
install -m 0644 "${SCRIPT_DIR}/litestream.yml" "${LITESTREAM_CONFIG}"

echo "==> Reloading systemd"
systemctl daemon-reload

# Create the env file with restrictive permissions only if it does not exist,
# so re-running setup.sh never clobbers operator-provided secrets.
if [[ ! -f "${ENV_FILE}" ]]; then
    echo "==> Creating ${ENV_FILE} template (owner-only readable)"
    umask 077
    cat > "${ENV_FILE}" <<'EOF'
# BuyWise environment file. Loaded by both buywise.service and litestream.service.
# Keep this file readable only by root (chmod 600).

# --- buywise-api (the application) ---
# SQLite connection URL. Must point at the Litestream-replicated database.
DATABASE_URL=sqlite:///var/lib/buywise/buywise.db
# TCP port the API binds on (0.0.0.0:PORT). Default 3000 if unset.
PORT=3000
# Log filter (tracing-subscriber EnvFilter syntax). Default "info" if unset.
RUST_LOG=info

# --- litestream (Cloudflare R2 credentials) ---
# R2 API token credentials (Cloudflare dashboard -> R2 -> Manage R2 API Tokens).
LITESTREAM_ACCESS_KEY_ID=
LITESTREAM_SECRET_ACCESS_KEY=

# --- weekly ingest (buywise-ingest.service / .timer) ---
# REQUIRED for the weekly ingest to succeed. BasketWatch API key. The
# ingest-basketwatch run (the primary, non-best-effort source) fails without
# it, which reds the whole weekly unit. Set it before the first weekly fire.
BASKETWATCH_API_KEY=
# Optional. BasketWatch API host. Defaults to basketwatch.p.rapidapi.com when
# unset; only set it to override (e.g. a different RapidAPI host).
#BASKETWATCH_API_HOST=basketwatch.p.rapidapi.com
# Optional. Source URL for the Lidl direct ingest adapter. If unset, the Lidl
# half of the weekly ingest is best-effort and is skipped without failing the
# timer; BasketWatch always runs. Set it to enable Lidl price ingest.
LIDL_FEED_URL=

# --- LLM resolution fallback (buywise-api; OPTIONAL, default OFF) ---
# When enabled, the API falls back to a cheap LLM call to resolve a list term
# that matches no catalogue item. The API runs fine with all of these unset;
# an unresolved term simply stays unresolved. Leave LLM_FALLBACK_ENABLED unset
# (or false) to keep the fallback off. When set to true, BOTH LLM_API_KEY and
# LLM_MODEL must be set for any request to be made; if either is missing the
# fallback silently stays disabled (no startup error).
#
# Master switch. "true" (case-insensitive) turns the fallback on; anything else
# (including unset) leaves it off.
#LLM_FALLBACK_ENABLED=false
# API key sent as a bearer token. Required when the fallback is enabled.
#LLM_API_KEY=
# Model name (no default; providers rotate cheap models often, so you pick one).
# Required when the fallback is enabled.
#LLM_MODEL=
# Optional provider identifier. Currently accepted but unused at runtime (every
# provider is treated as OpenAI-chat-completions-compatible); reserved for future
# per-provider defaults.
#LLM_PROVIDER=
# Optional chat-completions endpoint. Defaults to OpenAI's endpoint when unset;
# override for self-hosted / proxy / non-OpenAI deployments.
#LLM_BASE_URL=https://api.openai.com/v1/chat/completions
EOF
    chmod 600 "${ENV_FILE}"
    chown root:root "${ENV_FILE}"
else
    echo "==> ${ENV_FILE} already exists, leaving it untouched"
fi

# Enable (NOT start) the weekly ingest TIMER. Enabling persists it so it arms on
# the next boot; we deliberately do not `--now` it here. The timer has
# Persistent=true, so activating it after the Mon 03:00 calendar point has
# already elapsed makes systemd fire a catch-up run *immediately* — which on a
# fresh deploy would red buywise-ingest.service with MissingApiKey before the
# operator reaches the env-edit step. The post-env "Next steps" below starts the
# timer once BASKETWATCH_API_KEY is configured. The long-running
# buywise/litestream units are likewise left for the operator to enable.
echo "==> Enabling buywise-ingest.timer (weekly price ingest; not started yet)"
# This script targets a real systemd host. On a box without systemd as PID 1
# (container/chroot/nspawn) `enable` exits non-zero; guard it so a failure warns
# but does not abort the rest of the script (the "Next steps" guidance below)
# under `set -euo pipefail`.
systemctl enable buywise-ingest.timer \
    || echo "WARN: could not enable timer (no running systemd?); enable manually after setting the env file: systemctl enable --now buywise-ingest.timer" >&2

# Opt-in HTTPS reverse proxy. Installs the Caddyfile and enables/starts Caddy
# only once the operator has replaced the <hostname> placeholder and installed
# Caddy first: Caddy's automatic HTTPS would otherwise retry an invalid hostname
# forever. This mirrors how the other units are installed (`install` into place,
# then `systemctl enable`), except that Caddy is started here because serving
# HTTPS is the point of the step and there is no separate binary-copy step to
# wait for.
echo "==> Installing Caddy HTTPS reverse proxy (opt-in)"
CADDY_SOURCE="${SCRIPT_DIR}/Caddyfile"
if [[ ! -f "${CADDY_SOURCE}" ]]; then
    echo "    ${CADDY_SOURCE} not found; skipping the reverse-proxy step"
elif grep -q '^[[:space:]]*<hostname>[[:space:]]*{' "${CADDY_SOURCE}"; then
    echo "    placeholder site address <hostname> still present in ${CADDY_SOURCE}; NOT installing the Caddyfile or enabling Caddy."
    echo "    Set the hostname, install Caddy, then re-run setup.sh (see deploy/README.md section 7)."
elif ! command -v caddy >/dev/null 2>&1; then
    echo "    caddy binary not found; install Caddy first, then re-run setup.sh (see deploy/README.md section 7)." >&2
else
    install -d -m 0755 /etc/caddy
    install -m 0644 "${CADDY_SOURCE}" "${CADDY_CONFIG}"
    echo "    installed ${CADDY_CONFIG}"
    systemctl enable --now caddy \
        || echo "WARN: could not enable/start caddy (no running systemd?); enable manually: systemctl enable --now caddy" >&2
fi

cat <<EOF

==> Setup complete.

Next steps:

1. Edit ${ENV_FILE} and set the required variables:

     buywise-api (application):
       DATABASE_URL   SQLite URL for the DB (e.g. sqlite:///var/lib/buywise/buywise.db)
       PORT           TCP port to bind (default 3000)
       RUST_LOG       log filter (default "info")

     litestream (Cloudflare R2 backup):
       LITESTREAM_ACCESS_KEY_ID       R2 access key id
       LITESTREAM_SECRET_ACCESS_KEY   R2 secret access key

     weekly ingest:
       BASKETWATCH_API_KEY            REQUIRED for the weekly ingest. Without it
                                      the BasketWatch run fails and reds the
                                      whole weekly unit. Set it before the first
                                      weekly fire (Mon 03:00).
       BASKETWATCH_API_HOST           optional (defaults to
                                      basketwatch.p.rapidapi.com)
       LIDL_FEED_URL                  optional Lidl feed source URL. If unset,
                                      the Lidl half of the weekly ingest is
                                      skipped (best-effort); BasketWatch always
                                      runs.

     LLM resolution fallback (optional, default OFF; API runs fine without these):
       LLM_FALLBACK_ENABLED           optional master switch (default false).
       LLM_API_KEY                    required only when the fallback is enabled.
       LLM_MODEL                      required only when the fallback is enabled.
       LLM_PROVIDER                   optional (accepted but unused at runtime).
       LLM_BASE_URL                   optional (defaults to OpenAI's endpoint).

   The API starts and serves requests with no ingest or LLM vars set, but the
   price comparison stays empty until the DB has retailer offers (from the
   weekly ingest) AND product_matches rows. Seed the matches with
   'buywise-cli seed-matches' (see docs/phase0-results.md).

   Also edit ${LITESTREAM_CONFIG} and replace <ACCOUNT_ID> in the endpoint
   with your Cloudflare account id.

2. Ensure the binaries are installed:
       /usr/local/bin/buywise-api
       /usr/local/bin/buywise-cli   (needed by the weekly ingest timer)
       /usr/local/bin/litestream

3. Enable and start the services (only after the env file is set in step 1):

       sudo systemctl enable --now buywise litestream

   The weekly ingest timer (buywise-ingest.timer) was *enabled* by this script
   but NOT started, so it cannot fire a Persistent=true catch-up run before the
   env file exists. Start it now that BASKETWATCH_API_KEY is set:

       sudo systemctl start buywise-ingest.timer

   Inspect it and trigger a manual ingest run with:

       systemctl list-timers buywise-ingest.timer
       sudo systemctl start buywise-ingest.service   # run ingest now
       journalctl -u buywise-ingest -n 50 --no-pager

4. Verify:

       systemctl status buywise
       systemctl status litestream
       # Uses the default port 3000; adjust if you set PORT in ${ENV_FILE}.
       curl -i http://localhost:3000/api/auth/me

5. To serve the app to phones over HTTPS, follow deploy/README.md section 7
   (Caddy reverse proxy). This script installs the Caddyfile to
   ${CADDY_CONFIG} and enables caddy.service automatically once the
   hostname is set in deploy/Caddyfile and Caddy is installed.

EOF
