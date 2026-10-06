#!/usr/bin/env bash
#
# BuyWise deployment setup for a fresh Hetzner CAX11 (Ubuntu ARM) instance.
#
# Idempotent: safe to re-run. It creates the service user and data directory,
# installs the systemd units and the Litestream config, runs `daemon-reload`,
# and prints the env vars you must set in /etc/buywise/buywise.env before
# enabling the services. It enables no long-running unit: the buywise/litestream
# units are left for the operator to enable once the binaries are in place. It
# DOES install and enable the daily receipt-expiry timer
# (buywise-expire-receipts.timer), which needs no operator configuration and so
# enforces the 30-day receipt retention from the first deploy. The optional
# weekly ingest timer (buywise-ingest.timer) is installed but deliberately left
# disabled — the first deploy runs without price ingest, and a Persistent=true
# timer enabled before BASKETWATCH_API_KEY is set would fire a catch-up run and
# red the ingest with MissingApiKey. The operator enables the timer on purpose
# later (deploy/README.md, "Weekly price ingest"). It also
# installs and enables the Caddy HTTPS reverse proxy, then restarts it to load
# the installed config, but ONLY once the operator has replaced the <hostname>
# placeholder in Caddyfile and Caddy is already installed (see deploy/README.md
# section 7); it never enables the proxy with the placeholder still in place.
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
install -m 0644 "${SCRIPT_DIR}/systemd/buywise-expire-receipts.service" "${SYSTEMD_DIR}/buywise-expire-receipts.service"
install -m 0644 "${SCRIPT_DIR}/systemd/buywise-expire-receipts.timer" "${SYSTEMD_DIR}/buywise-expire-receipts.timer"

echo "==> Installing Litestream config to ${LITESTREAM_CONFIG}"
# 0644 is intentional: litestream.yml contains no secrets (only placeholders);
# the R2 keys come from /etc/buywise/buywise.env (mode 600).
install -m 0644 "${SCRIPT_DIR}/litestream.yml" "${LITESTREAM_CONFIG}"

echo "==> Reloading systemd"
systemctl daemon-reload

# Enable the daily receipt-expiry timer. Unlike the weekly ingest timer, this
# one is enabled automatically: it needs no operator configuration (only the
# local DB and receipt directory), so the 30-day receipt retention is enforced
# from the first deploy. `enable --now` starts the timer, not the service — the
# first sweep runs at the next 03:30 (or on the next boot if that was missed).
# Guarded like the Caddy step so a host without a running systemd only warns
# instead of failing under `set -e`.
echo "==> Enabling buywise-expire-receipts.timer (daily receipt-expiry sweep)"
systemctl enable --now buywise-expire-receipts.timer \
    && echo "==> Installed and enabled buywise-expire-receipts.timer; the daily receipt-expiry sweep runs without operator action" \
    || echo "WARN: could not enable/start buywise-expire-receipts.timer (no running systemd?); enable it manually: systemctl enable --now buywise-expire-receipts.timer" >&2

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

# --- weekly ingest (buywise-ingest.service / .timer; OPTIONAL, disabled) ---
# The timer is installed but NOT enabled by setup.sh, so the first deploy runs
# without price ingest. Enable it later, once the live-savings validation
# (issue #109) has confirmed supplier access, terms and cost:
#   sudo systemctl enable --now buywise-ingest.timer
# (deploy/README.md, "Weekly price ingest").
#
# REQUIRED once you enable the ingest. BasketWatch API key. The
# ingest-basketwatch run (the primary, non-best-effort source) fails without
# it, which reds the whole weekly unit. Set it before enabling the timer.
BASKETWATCH_API_KEY=
# Optional. BasketWatch API host. Defaults to basketwatch.p.rapidapi.com when
# unset; only set it to override (e.g. a different RapidAPI host).
#BASKETWATCH_API_HOST=basketwatch.p.rapidapi.com
# Optional. Source URL for the Lidl direct ingest adapter. There is no default
# and no verified Lidl feed yet. If unset, the Lidl half of the weekly ingest is
# best-effort and is skipped without failing the timer; BasketWatch always runs.
LIDL_FEED_URL=

# --- receipt expiry (buywise-expire-receipts.service / .timer; enabled) ---
# The daily receipt-expiry timer is installed and ENABLED by setup.sh, so the
# 30-day receipt retention runs with no operator action; the sweep itself needs
# no configuration. Receipt images are kept for 30 days and then deleted.
# Optional. Directory holding the receipt image files. Defaults to a `receipts/`
# directory next to the database (/var/lib/buywise/receipts) when unset; set it
# only if you store receipt images somewhere else.
#RECEIPT_DIR=/var/lib/buywise/receipts

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

# Do NOT enable the weekly ingest timer. The units are installed above, but the
# timer stays disabled on purpose: the first deploy runs without price ingest
# (it needs a paid BasketWatch key whose terms and cost are still being
# validated against issue #109), and the timer has Persistent=true, so an
# enabled-but-unconfigured timer fires a catch-up run at the next boot and reds
# buywise-ingest.service with MissingApiKey. The operator enables it on purpose
# once the key is set; see deploy/README.md, "Weekly price ingest".
echo "==> Installed buywise-ingest.timer (weekly price ingest) but did NOT enable it"
echo "    Enable it later with: sudo systemctl enable --now buywise-ingest.timer"

# Opt-in HTTPS reverse proxy. Installs the Caddyfile, enables Caddy, and
# restarts it to load the new config only once the operator has replaced the
# <hostname> placeholder and installed Caddy first: automatic HTTPS would
# otherwise retry an invalid hostname forever.
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
    systemctl enable caddy && systemctl restart caddy \
        || echo "WARN: could not enable/restart caddy (no running systemd?); enable and restart manually: systemctl enable caddy && systemctl restart caddy" >&2
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

     weekly ingest (optional; the units are installed but NOT enabled):
       BASKETWATCH_API_KEY            REQUIRED to enable the ingest. Without it
                                      the BasketWatch run fails and reds the
                                      whole weekly unit. Leave it unset for the
                                      first deploy; set it only once the
                                      live-savings validation (issue #109) has
                                      confirmed supplier access, terms and cost.
       BASKETWATCH_API_HOST           optional (defaults to
                                      basketwatch.p.rapidapi.com)
       LIDL_FEED_URL                  optional Lidl feed source URL. There is no
                                      default and no verified feed yet; if unset
                                      the Lidl half of the ingest is skipped
                                      (best-effort); BasketWatch always runs.

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
   with your Cloudflare account id, and make sure the R2 token in
   ${ENV_FILE} is scoped to the buywise-backup bucket only (see
   deploy/README.md section 4.2).

2. Ensure the binaries are installed:
       /usr/local/bin/buywise-api
       /usr/local/bin/buywise-cli   (needed by the ingest and receipt-expiry timers)
       /usr/local/bin/litestream

   And the built frontend is in place at
   /var/lib/buywise/frontend/build (see deploy/README.md section 1).

3. Enable and start the services (only after the env file is set in step 1):

       sudo systemctl enable --now buywise litestream

   The weekly ingest timer (buywise-ingest.timer) was installed but NOT enabled:
   the first deploy runs without price ingest. Enable it only after issue #109
   has confirmed supplier access, terms and cost, and after setting
   BASKETWATCH_API_KEY:

       sudo systemctl enable --now buywise-ingest.timer
       sudo systemctl start buywise-ingest.service   # run an ingest now
       journalctl -u buywise-ingest -n 50 --no-pager

   The daily receipt-expiry timer (buywise-expire-receipts.timer) was installed
   and ENABLED by this script: it deletes receipt images and rows past their
   30-day expiry every day at 03:30 and needs no configuration. If receipt
   images live outside the default directory (/var/lib/buywise/receipts), set
   RECEIPT_DIR in ${ENV_FILE} (see deploy/README.md, "Daily receipt-expiry
   sweep"). Inspect or trigger it with:

       systemctl list-timers buywise-expire-receipts.timer
       sudo systemctl start buywise-expire-receipts.service   # sweep now
       journalctl -u buywise-expire-receipts -n 50 --no-pager

4. Verify:

       systemctl status buywise
       systemctl status litestream
       # One-command check: health, PWA manifest + service worker, and, with
       # credentials, an authenticated list read and a short SSE stream.
       # Adjust the port if you set PORT in ${ENV_FILE}.
       BUYWISE_SMOKE_EMAIL=owner@example.com \
       BUYWISE_SMOKE_PASSWORD='<owner password>' \
         ${SCRIPT_DIR}/smoke.sh http://localhost:3000

5. To serve the app to phones over HTTPS, follow deploy/README.md section 7
   (Caddy reverse proxy). This script installs the Caddyfile to
   ${CADDY_CONFIG}, enables caddy.service, and restarts it to load the config
   once the hostname is set in deploy/Caddyfile and Caddy is installed.

EOF
