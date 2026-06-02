#!/usr/bin/env bash
#
# BuyWise deployment setup for a fresh Hetzner CAX11 (Ubuntu ARM) instance.
#
# Idempotent: safe to re-run. It creates the service user and data directory,
# installs the systemd units and the Litestream config, runs `daemon-reload`,
# and prints the env vars you must set in /etc/buywise/buywise.env before
# enabling the services. It does NOT enable or start the services (the binaries
# must be in place first); it prints the exact commands to do so.
#
# Prerequisites (see deploy/README.md): the buywise-api and litestream binaries
# must already be copied to /usr/local/bin/.
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
EOF
    chmod 600 "${ENV_FILE}"
    chown root:root "${ENV_FILE}"
else
    echo "==> ${ENV_FILE} already exists, leaving it untouched"
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

   Also edit ${LITESTREAM_CONFIG} and replace <ACCOUNT_ID> in the endpoint
   with your Cloudflare account id.

2. Ensure the binaries are installed:
       /usr/local/bin/buywise-api
       /usr/local/bin/litestream

3. Enable and start the services:

       sudo systemctl enable --now buywise litestream

4. Verify:

       systemctl status buywise
       systemctl status litestream
       # Uses the default port 3000; adjust if you set PORT in ${ENV_FILE}.
       curl -i http://localhost:3000/api/auth/me

EOF
