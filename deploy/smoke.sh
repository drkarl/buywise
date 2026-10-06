#!/usr/bin/env bash
#
# BuyWise post-deploy smoke test (issue #141).
#
# Usage: deploy/smoke.sh <base-url>
#
# Verifies that a running BuyWise deployment is genuinely ready, not just that a
# listener answers:
#
#   1. GET <base>/api/health returns 200 and reports ok — the API's own check
#      that the database is reachable and every migration is applied.
#   2. GET <base>/manifest.webmanifest serves the built PWA manifest.
#   3. GET <base>/sw.js serves the built service worker.
#   4. If credentials are supplied (see below), the script logs in, reads the
#      authenticated list index, and holds a short SSE connection open expecting
#      a frame — proving auth, session handling and the live-sync stream work
#      through whatever sits in front of the API.
#
# Credentials are read only from the environment and are never printed: the
# login body goes over stdin (not the curl command line, so it is not visible in
# `ps`) and the session cookies live in a mode-0600 temp jar, not in an argv
# header.
#
# Environment:
#   BUYWISE_SMOKE_EMAIL        account email for the authenticated checks
#   BUYWISE_SMOKE_PASSWORD     that account's password
#   BUYWISE_SMOKE_LIST_ID      optional; list id for the SSE check (defaults to
#                              the first list the account can see)
#   BUYWISE_SMOKE_SSE_TIMEOUT  optional; seconds to wait for an SSE frame
#                              (default 35, just over the API's 30s heartbeat)
#   BUYWISE_SMOKE_HTTP_TIMEOUT optional; per-request curl timeout (default 10)
#
# Both BUYWISE_SMOKE_EMAIL and BUYWISE_SMOKE_PASSWORD must be set to run the
# authenticated checks; leaving both unset runs the anonymous checks only.
#
# Exits 0 when every check passes, non-zero with a clear FAIL line otherwise.

set -eu

usage() {
	cat <<'EOF'
BuyWise post-deploy smoke test.

Usage:
  deploy/smoke.sh <base-url>

  <base-url>  Base URL of the running deployment, e.g. https://buywise.example.com
              (a trailing slash is ignored). Use http://127.0.0.1:3000 to test the
              loopback listener directly.

Checks the API health endpoint, the PWA manifest and service worker, and, when
credentials are provided, an authenticated list read and a short SSE connection.

Environment:
  BUYWISE_SMOKE_EMAIL         account email for the authenticated checks
  BUYWISE_SMOKE_PASSWORD      that account's password
  BUYWISE_SMOKE_LIST_ID       optional list id for the SSE check
  BUYWISE_SMOKE_SSE_TIMEOUT   seconds to wait for an SSE frame (default 35)
  BUYWISE_SMOKE_HTTP_TIMEOUT  per-request curl timeout (default 10)

Credentials are read only from the environment and are never printed.
EOF
}

case "${1:-}" in
	-h | --help)
		usage
		exit 0
		;;
	"")
		printf 'smoke.sh: missing <base-url> argument\n\n' >&2
		usage >&2
		exit 2
		;;
esac
if [ "$#" -ne 1 ]; then
	printf 'smoke.sh: expected exactly one <base-url> argument, got %s\n\n' "$#" >&2
	usage >&2
	exit 2
fi
if ! command -v curl >/dev/null 2>&1; then
	printf 'smoke.sh: curl is required but was not found on PATH\n' >&2
	exit 2
fi

base_url=${1%/}
http_timeout=${BUYWISE_SMOKE_HTTP_TIMEOUT:-10}
sse_timeout=${BUYWISE_SMOKE_SSE_TIMEOUT:-35}

umask 077
tmpdir=$(mktemp -d)
cleanup() { rm -rf "${tmpdir}"; }
trap cleanup EXIT INT TERM

fail() {
	printf 'FAIL: %s\n' "$*" >&2
	exit 1
}
pass() {
	printf 'ok: %s\n' "$*"
}

# GET a URL into <outfile>, echoing "<http-code> <content-type>". Extra curl
# arguments (e.g. -b <jar>) follow the URL. A transport failure (connection
# refused, DNS, TLS) aborts the pipeline so the caller's `|| fail` runs.
fetch() {
	# shellcheck disable=SC2068  # extra args are a deliberate passthrough
	curl -sS --max-time "${http_timeout}" -o "$1" -w '%{http_code} %{content_type}' \
		${@:3} "$2"
}

# HTTP status code of the last `fetch` result ("200 text/html; charset=utf-8").
code_of() { printf '%s' "${1%% *}"; }

# Split a <code> <content-type> string and reject an HTML body, which is what the
# SPA fallback serves for a missing asset — the exact failure this catches.
expect_asset() {
	label=$1
	path=$2
	outfile=$3
	resp=$4
	code=$(code_of "${resp}")
	ctype=${resp#* }
	[ "${code}" = "200" ] || fail "${label}: GET ${path} returned HTTP ${code} (expected 200); is the frontend build installed?"
	case "${ctype}" in
	*html*)
		fail "${label}: GET ${path} served HTML (${ctype}), not the built asset — the frontend build is missing or not in the API's working directory"
		;;
	esac
	[ -s "${outfile}" ] || fail "${label}: GET ${path} returned an empty body"
}

printf '==> Smoke test against %s\n' "${base_url}"

# --- 1. API health ---------------------------------------------------------
printf '==> API health (%s/api/health)\n' "${base_url}"
if ! resp=$(fetch "${tmpdir}/health.json" "${base_url}/api/health"); then
	fail "cannot reach ${base_url}/api/health — is the API running and the URL reachable?"
fi
if [ "$(code_of "${resp}")" != "200" ]; then
	fail "GET /api/health returned HTTP $(code_of "${resp}") (expected 200): $(cat "${tmpdir}/health.json")"
fi
case "$(cat "${tmpdir}/health.json")" in
*'"status":"ok"'*) : ;;
*) fail "GET /api/health reported the API not ready: $(cat "${tmpdir}/health.json")" ;;
esac
pass "API health reports ok"

# --- 2 & 3. PWA assets -----------------------------------------------------
printf '==> PWA assets\n'
if ! resp=$(fetch "${tmpdir}/manifest.webmanifest" "${base_url}/manifest.webmanifest"); then
	fail "cannot reach ${base_url}/manifest.webmanifest"
fi
expect_asset "manifest" "/manifest.webmanifest" "${tmpdir}/manifest.webmanifest" "${resp}"
grep -q '"name"' "${tmpdir}/manifest.webmanifest" ||
	fail "/manifest.webmanifest is not a web app manifest (no \"name\" field)"
pass "PWA manifest served"

if ! resp=$(fetch "${tmpdir}/sw.js" "${base_url}/sw.js"); then
	fail "cannot reach ${base_url}/sw.js"
fi
expect_asset "service worker" "/sw.js" "${tmpdir}/sw.js" "${resp}"
grep -Eq 'precache|importScripts|caches' "${tmpdir}/sw.js" ||
	fail "/sw.js does not look like a service worker (no cache or precache call)"
pass "service worker served"

# --- 4. Authenticated checks ----------------------------------------------
email=${BUYWISE_SMOKE_EMAIL:-}
password=${BUYWISE_SMOKE_PASSWORD:-}
if [ -z "${email}" ] && [ -z "${password}" ]; then
	printf '==> Authenticated checks skipped (set BUYWISE_SMOKE_EMAIL and BUYWISE_SMOKE_PASSWORD to enable)\n'
	printf 'SMOKE TEST PASSED\n'
	exit 0
fi
if [ -z "${email}" ] || [ -z "${password}" ]; then
	fail "set both BUYWISE_SMOKE_EMAIL and BUYWISE_SMOKE_PASSWORD (or neither)"
fi

# JSON-escape a value supplied via the environment (backslash, quote and the
# whitespace controls a password may contain).
json_string() {
	s=$1
	s=${s//\\/\\\\}
	s=${s//\"/\\\"}
	s=${s//$'\n'/\\n}
	s=${s//$'\r'/\\r}
	s=${s//$'\t'/\\t}
	printf '"%s"' "${s}"
}

printf '==> Authenticated checks (BUYWISE_SMOKE_EMAIL is set)\n'
login_body=$(printf '{"email":%s,"password":%s}' "$(json_string "${email}")" "$(json_string "${password}")")
if ! resp=$(printf '%s' "${login_body}" | curl -sS --max-time "${http_timeout}" \
	-o "${tmpdir}/login.json" -w '%{http_code}' -c "${tmpdir}/cookies" \
	-H 'Content-Type: application/json' --data @- "${base_url}/api/auth/login"); then
	fail "cannot reach ${base_url}/api/auth/login"
fi
if [ "${resp}" != "200" ]; then
	# Never echo the login body: on a failure it is a generic error, but on some
	# paths it could carry account detail.
	fail "login failed with HTTP ${resp} (expected 200) — check BUYWISE_SMOKE_EMAIL/BUYWISE_SMOKE_PASSWORD"
fi
pass "logged in"

if ! resp=$(fetch "${tmpdir}/lists.json" "${base_url}/api/lists" -b "${tmpdir}/cookies"); then
	fail "cannot reach ${base_url}/api/lists"
fi
if [ "$(code_of "${resp}")" != "200" ]; then
	fail "GET /api/lists returned HTTP $(code_of "${resp}") (expected 200) — is the session accepted?"
fi
grep -q '^\[' "${tmpdir}/lists.json" || fail "GET /api/lists did not return a JSON array"
pass "authenticated list read succeeded"

list_id=${BUYWISE_SMOKE_LIST_ID:-}
if [ -z "${list_id}" ]; then
	list_id=$(grep -o '"id":"[^"]*"' "${tmpdir}/lists.json" | head -n 1 | cut -d'"' -f4 || true)
fi
if [ -z "${list_id}" ]; then
	printf 'SKIP: the account can see no list, so the SSE check cannot run; set BUYWISE_SMOKE_LIST_ID to target one\n'
	printf 'SMOKE TEST PASSED\n'
	exit 0
fi

printf '==> SSE stream (%s/api/sse/%s, waiting up to %ss)\n' "${base_url}" "${list_id}" "${sse_timeout}"
curl_rc=0
# curl exits 28 when --max-time cuts the (never-ending) stream; that is expected,
# so accept 0 and 28 and judge by whether a frame arrived.
curl -sS -N --max-time "${sse_timeout}" -b "${tmpdir}/cookies" \
	"${base_url}/api/sse/${list_id}" >"${tmpdir}/sse.txt" 2>"${tmpdir}/sse.err" || curl_rc=$?
if [ "${curl_rc}" -ne 0 ] && [ "${curl_rc}" -ne 28 ]; then
	fail "SSE connection to /api/sse/${list_id} failed (curl exit ${curl_rc})"
fi
grep -q '^event: ' "${tmpdir}/sse.txt" ||
	fail "no SSE frame arrived within ${sse_timeout}s on /api/sse/${list_id} (expected a mutation or the 30s heartbeat — is a proxy buffering the stream?)"
pass "SSE stream delivered a frame"

printf 'SMOKE TEST PASSED\n'
