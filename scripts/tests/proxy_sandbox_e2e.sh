#!/usr/bin/env bash
# Proxy sandbox end-to-end verification (read path v1).
#
# Verifies, in order:
#   1. Server startup brings up the resident proxy sandbox; the proxy is
#      usable and can execute basic commands (ls).
#   2. First pause of a business sandbox creates a new ublk device from the
#      staged snapshot config, adopts it into a proxy slot, mounts it inside
#      the proxy guest, and the mounted data is readable from the proxy.
#   3. POST /sandboxes/{id}/proxy-read returns the correct content for the
#      paused target.
#   4. After resume + write actions (dirty pages, no explicit sync) + pause,
#      the server rebinds the slot to the new snapshot generation.
#   5. proxy-read observes the freshly written (dirty-page) content and the
#      new binding generation.
#
# Also verifies the UNBIND path on sandbox delete, and the paused-only
# contract of proxy-read (409 while the target is running).
#
# Prerequisites:
#   - Linux host with /dev/kvm access for the current user
#   - Local registry at 127.0.0.1:5000 with the test image (see
#     PROXY_E2E_IMAGE below; the AgentENV replay workflow uses the same
#     registry)
#   - curl, jq, python3
#   - sudo (prompts once up front; run-with-capabilities.sh uses the cached
#     credentials to grant CAP_NET_ADMIN / CAP_SYS_ADMIN to the server)
#   - AgentENV runtime deps already provisioned (default
#     AENV_DEPS_PATH=/var/lib/aenv/deps: firecracker, kernel, tools drive,
#     regctl)
#
# Environment knobs:
#   AENV_PORT            Port for the test server (default: 18200)
#   PROXY_E2E_IMAGE      Image for the proxy and business sandbox
#                        (default: localhost:5000/swe-python/joke2k__faker-2007:latest)
#   PROXY_E2E_HOME_DIR   Persistent AENV home (default: /tmp/proxy-sandbox-e2e).
#                        Reused across runs so the image conversion and the
#                        proxy template stay warm; set CLEAN_HOME=1 to wipe it.
#   AENV_DEPS_PATH       Runtime deps root (default: /var/lib/aenv/deps)
#   SKIP_BUILD           Set to 1 to skip building server/aenv binaries
#   KEEP_SERVER          Set to 1 to leave the server running on success
#
# Usage:
#   bash scripts/tests/proxy_sandbox_e2e.sh
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

AENV_PORT="${AENV_PORT:-18200}"
PROXY_E2E_IMAGE="${PROXY_E2E_IMAGE:-localhost:5000/swe-python/joke2k__faker-2007:latest}"
PROXY_E2E_HOME_DIR="${PROXY_E2E_HOME_DIR:-/tmp/proxy-sandbox-e2e}"
AENV_DEPS_PATH="${AENV_DEPS_PATH:-/var/lib/aenv/deps}"
TEST_API_KEY="e2b_proxye2etest0000000000000000000000000000000000000000"

SERVER_START_TIMEOUT="${SERVER_START_TIMEOUT:-120}"
# First run pulls + converts the image and captures the proxy template.
PROXY_START_TIMEOUT="${PROXY_START_TIMEOUT:-900}"
SANDBOX_READY_TIMEOUT="${SANDBOX_READY_TIMEOUT:-180}"
BIND_TIMEOUT="${BIND_TIMEOUT:-60}"
PROXY_READ_RETRY_TIMEOUT="${PROXY_READ_RETRY_TIMEOUT:-30}"
DIRTY_WRITE_MB="${DIRTY_WRITE_MB:-8}"

AENV_HOME_PATH="${PROXY_E2E_HOME_DIR}/home"
AENV_RUNTIME_PATH="${PROXY_E2E_HOME_DIR}/run"
SERVER_LOG="${PROXY_E2E_HOME_DIR}/server.log"
DAEMON_LOG="${AENV_HOME_PATH}/logs/ublk-daemon.log"
AENV_URL="http://127.0.0.1:${AENV_PORT}"

SERVER_BIN="${REPO_ROOT}/target/release/server"
DAEMON_BIN="${REPO_ROOT}/target/release/uvm-ublk-daemon"
AENV_BIN="${REPO_ROOT}/target/release/aenv"
AENV_CLI_HOME="${PROXY_E2E_HOME_DIR}/aenv-cli-home"

PASS_COUNT=0
FAIL_COUNT=0
SERVER_PID=""
BUSINESS_SANDBOX_ID=""
PROXY_SANDBOX_ID=""

# ── Reporting ────────────────────────────────────────────────────────────────

pass() { PASS_COUNT=$((PASS_COUNT + 1)); printf '  [PASS] %s\n' "$1"; }
fail() {
  FAIL_COUNT=$((FAIL_COUNT + 1))
  printf '  [FAIL] %s\n' "$1"
  [[ -n "${2:-}" ]] && printf '         got: %s want: %s\n' "$2" "${3:-}"
}

note() { printf '  [..]   %s\n' "$1"; }

section() { printf '\n== %s ==\n' "$1"; }

die() {
  printf 'FATAL: %s\n' "$1" >&2
  printf '\nLast server log lines:\n' >&2
  tail -n 40 "${SERVER_LOG}" >&2 || true
  exit 1
}

on_exit() {
  local exit_code=$?
  trap - EXIT INT TERM
  if ((exit_code != 0)); then
    printf '\nTest aborted; last server log lines:\n' >&2
    tail -n 60 "${SERVER_LOG}" >&2 || true
  fi
  # cleanup_server is defined further down; early exits (e.g. the root check
  # or missing prerequisites) must not reference it before its definition.
  if declare -f cleanup_server >/dev/null 2>&1; then
    cleanup_server
  fi
  exit "$exit_code"
}
trap on_exit EXIT INT TERM

# ── Prerequisites ────────────────────────────────────────────────────────────

require_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }
require_cmd curl
require_cmd jq
require_cmd python3

[[ -e /dev/kvm ]] || die "/dev/kvm not found"
[[ -r /dev/kvm ]] || die "/dev/kvm is not readable by $(id -un)"
[[ -d "${AENV_DEPS_PATH}" ]] || die "runtime deps not found at ${AENV_DEPS_PATH} (set AENV_DEPS_PATH)"
if [[ ${EUID} -eq 0 ]]; then
  # Invoked as root (e.g. from a root shell): re-exec as the repo owner so
  # all test state stays user-owned and the sudo elevation below works
  # unchanged. `sudo -u` from root needs no password; the later `sudo -v`
  # prompts for that user's password normally.
  repo_owner="$(stat -c '%U' "${SCRIPT_DIR}" 2>/dev/null || true)"
  if [[ -n "${repo_owner}" && "${repo_owner}" != "root" ]] && id -u "${repo_owner}" >/dev/null 2>&1; then
    note "invoked as root; re-execing as ${repo_owner} (repo owner)"
    reexec_env=()
    for var in AENV_PORT PROXY_E2E_IMAGE PROXY_E2E_HOME_DIR AENV_DEPS_PATH \
               SERVER_START_TIMEOUT PROXY_START_TIMEOUT SANDBOX_READY_TIMEOUT \
               BIND_TIMEOUT PROXY_READ_RETRY_TIMEOUT DIRTY_WRITE_MB; do
      reexec_env+=("${var}=${!var}")
    done
    for var in SKIP_BUILD KEEP_SERVER CLEAN_HOME; do
      if [[ -n "${!var:-}" ]]; then
        reexec_env+=("${var}=${!var}")
      fi
    done
    exec sudo -u "${repo_owner}" env "${reexec_env[@]}" bash "$0" "$@"
  fi
  printf 'FATAL: could not find a non-root repo owner to run as; invoke this script as a regular user\n' >&2
  exit 1
fi

# Elevate credentials up front: run-with-capabilities.sh invokes sudo from a
# background job, where an interactive password prompt would hang the run.
note "requesting sudo credentials up front (needed for CAP_NET_ADMIN/CAP_SYS_ADMIN)"
sudo -v || die "sudo is required to run the server with its capability set"

if [[ "${CLEAN_HOME:-0}" == "1" ]]; then
  note "wiping ${PROXY_E2E_HOME_DIR} (CLEAN_HOME=1)"
  rm -rf "${PROXY_E2E_HOME_DIR}"
fi
mkdir -p "${AENV_HOME_PATH}" "${AENV_RUNTIME_PATH}" "${PROXY_E2E_HOME_DIR}"

# ── Build ────────────────────────────────────────────────────────────────────

if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  section "Building server and aenv (release)"
  (cd "${REPO_ROOT}" && cargo build --release -p agentenv --bin server -p aenv -p uvm-ublk-daemon) \
    || die "cargo build failed"
fi
[[ -x "${SERVER_BIN}" ]] || die "server binary missing: ${SERVER_BIN} (build first or set SKIP_BUILD=1)"
[[ -x "${AENV_BIN}" ]] || die "aenv binary missing: ${AENV_BIN}"
# The fresh test home has no provisioned daemon binary; point the server at the
# repo-built one explicitly (same override the Makefile test targets use).
[[ -x "${DAEMON_BIN}" ]] || die "ublk daemon binary missing: ${DAEMON_BIN}"

# ── Config ───────────────────────────────────────────────────────────────────

TEST_CONFIG="${PROXY_E2E_HOME_DIR}/config.toml"
python3 - "$REPO_ROOT/config/default.toml" "$TEST_CONFIG" <<'PYEOF'
import re
import sys

src_path, dst_path = sys.argv[1], sys.argv[2]
with open(src_path) as f:
    lines = f.readlines()

in_proxy = False
flipped = False
for i, line in enumerate(lines):
    if line.startswith("["):
        in_proxy = line.strip() == "[proxy]"
        continue
    if in_proxy and re.match(r"^enabled\s*=\s*false\s*$", line):
        lines[i] = "enabled = true\n"
        flipped = True
        break

if not flipped:
    sys.exit("could not find [proxy] enabled=false in the source config")

with open(dst_path, "w") as f:
    f.writelines(lines)
PYEOF
note "test config written to ${TEST_CONFIG} ([proxy] enabled)"

# aenv CLI credentials are stored under $HOME/.config/aenv/credentials; give
# the CLI an isolated HOME so the real user credentials are never touched.
mkdir -p "${AENV_CLI_HOME}/.config/aenv"
cat > "${AENV_CLI_HOME}/.config/aenv/credentials" <<EOF
url = "${AENV_URL}"
api_key = "${TEST_API_KEY}"
EOF

# ── Server lifecycle ─────────────────────────────────────────────────────────

# Kill firecracker VMs and the ublk daemon leaked from earlier aborted runs.
# A force-killed server leaves its VMs alive (own process groups), and the
# ublk daemon then hangs on stop_dev while the VMs still hold the devices,
# pinning the daemon socket and breaking the next server start. Patterns are
# scoped to this test's home so production instances are never matched.
kill_leftover_runtime() {
  pkill -TERM -f "firecracker.*${PROXY_E2E_HOME_DIR}" 2>/dev/null || true
  pkill -TERM -f "uvm-ublk-daemon.*${PROXY_E2E_HOME_DIR}" 2>/dev/null || true
  for ((i = 0; i < 20; i++)); do
    if ! pgrep -f "firecracker.*${PROXY_E2E_HOME_DIR}" >/dev/null 2>&1 \
      && ! pgrep -f "uvm-ublk-daemon.*${PROXY_E2E_HOME_DIR}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  pkill -KILL -f "firecracker.*${PROXY_E2E_HOME_DIR}" 2>/dev/null || true
  pkill -KILL -f "uvm-ublk-daemon.*${PROXY_E2E_HOME_DIR}" 2>/dev/null || true
  sleep 1
}

cleanup_server() {
  if [[ -n "${KEEP_SERVER:-}" && ${PASS_COUNT} -gt 0 && ${FAIL_COUNT} -eq 0 && -z "${BUSINESS_SANDBOX_ID:-}" ]]; then
    note "KEEP_SERVER=1: leaving server running (log: ${SERVER_LOG})"
    return
  fi
  note "stopping server"
  # The server itself runs as the invoking user; the wrapper PID is root-owned.
  # Prefer non-interactive sudo, fall back to killing the port listener as the
  # current user (the listener is owned by us), never hang on a sudo prompt.
  # fuser -k defaults to SIGKILL; -TERM keeps the server's graceful shutdown
  # (stops VMs, shuts the daemon) intact.
  if [[ -n "${SERVER_PID}" ]]; then
    sudo -n kill "${SERVER_PID}" 2>/dev/null || kill "${SERVER_PID}" 2>/dev/null || true
    for ((i = 0; i < 10; i++)); do
      ps -p "${SERVER_PID}" >/dev/null 2>&1 || break
      sleep 1
    done
  fi
  sudo -n fuser -k -TERM "${AENV_PORT}/tcp" 2>/dev/null || fuser -k -TERM "${AENV_PORT}/tcp" 2>/dev/null || true
  sleep 2
  kill_leftover_runtime
}

start_server() {
  section "Starting AgentENV server on ${AENV_URL}"
  sudo -n fuser -k -TERM "${AENV_PORT}/tcp" 2>/dev/null || fuser -k -TERM "${AENV_PORT}/tcp" 2>/dev/null || true
  kill_leftover_runtime
  : > "${SERVER_LOG}"

  # JSON log format keeps the script's jq assertions stable.
  # Strip shell-inherited HTTP proxy variables: run-with-capabilities.sh
  # self-elevates via `sudo -E`, which would otherwise pass them to the
  # server, and reqwest routes envd health probes (10.11/169.254) through
  # the proxy — the proxy cannot reach sandbox-internal ranges and every
  # probe times out ("timed out waiting for envd").
  env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
      -u all_proxy -u ALL_PROXY \
      no_proxy="localhost,127.0.0.1,169.254.0.0/16,10.11.0.0/16,10.12.0.0/16" \
      NO_PROXY="localhost,127.0.0.1,169.254.0.0/16,10.11.0.0/16,10.12.0.0/16" \
    "${REPO_ROOT}/scripts/run-with-capabilities.sh" env \
    "API_ADDR=127.0.0.1:${AENV_PORT}" \
    "AENV_CONFIG_PATH=${TEST_CONFIG}" \
    "AENV_HOME_PATH=${AENV_HOME_PATH}" \
    "AENV_RUNTIME_PATH=${AENV_RUNTIME_PATH}" \
    "AENV_DEPS_PATH=${AENV_DEPS_PATH}" \
    "AENV_UBLK_DAEMON_BINARY_PATH=${DAEMON_BIN}" \
    "AENV_API_KEY=${TEST_API_KEY}" \
    "AENV_PROXY_IMAGE=${PROXY_E2E_IMAGE}" \
    "RUST_LOG=agentenv=debug,uvm_ublk=info,envd=info" \
    "AENV_LOG_FORMAT=json" \
    "${SERVER_BIN}" >> "${SERVER_LOG}" 2>&1 &
  SERVER_PID=$!

  for ((i = 0; i < SERVER_START_TIMEOUT; i++)); do
    curl -sf --noproxy '*' "${AENV_URL}/health" >/dev/null 2>&1 && return 0
    # The background PID is the root-owned sudo wrapper; kill -0 would fail
    # with EPERM from this user, so check existence via ps instead.
    ps -p "${SERVER_PID}" >/dev/null 2>&1 || die "server process exited during startup"
    sleep 1
  done
  die "server did not become healthy within ${SERVER_START_TIMEOUT}s"
}

# ── HTTP helpers ─────────────────────────────────────────────────────────────

api() {
  local method="$1" path="$2" body="${3:-}"
  local args=(-s --noproxy '*' -X "${method}"
    -H "X-API-Key: ${TEST_API_KEY}"
    -H "Content-Type: application/json"
    -w '\n%{http_code}')
  [[ -n "$body" ]] && args+=(-d "$body")
  local raw
  raw="$(curl "${args[@]}" "${AENV_URL}${path}")" || raw=""
  HTTP_BODY="$(printf '%s' "$raw" | sed '$d')"
  HTTP_STATUS="$(printf '%s' "$raw" | tail -n1)"
}

get_sandbox_field() { # id jq_field
  api GET "/sandboxes/$1"
  [[ "${HTTP_STATUS}" == "200" ]] && jq -r "$2 // empty" <<<"${HTTP_BODY}" || echo ""
}

get_sandbox_state() { get_sandbox_field "$1" '.state'; }

wait_for_sandbox_state() { # id state timeout_secs
  local id="$1" want="$2" timeout="${3:-30}"
  for ((i = 0; i < timeout * 2; i++)); do
    [[ "$(get_sandbox_state "$id")" == "$want" ]] && return 0
    sleep 0.5
  done
  return 1
}

# ── Log helpers (server writes JSON-formatted tracing) ──────────────────────

# wait_for_log_event <message> [jq_filter] [timeout_secs]
#   jq_filter is appended inside select(...) context, e.g. '.fields.generation==2'
# wait_for_log_event <message> [jq_filter] [timeout_secs]
# Anchors match by prefix (startswith) so "resident proxy sandbox started"
# also matches its "via cold start"/"from template" suffixed variants.
wait_for_log_event() {
  local message="$1" filter="${2:-.}" timeout="${3:-${BIND_TIMEOUT}}"
  for ((i = 0; i < timeout; i++)); do
    if grep -F "\"${message}" "${SERVER_LOG}" 2>/dev/null \
      | jq -e "select(.fields.message | startswith(\"${message}\")) | ${filter}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

log_has_event() { # message [jq_filter]
  grep -F "\"$1" "${SERVER_LOG}" 2>/dev/null \
    | jq -e "select(.fields.message | startswith(\"$1\")) | ${2:-.}" >/dev/null 2>&1
}

ublk_device_count() { ls /dev/ublkb[0-9]* 2>/dev/null | wc -l; }

# ── Sandbox exec (aenv CLI with isolated HOME) ──────────────────────────────

# Runs a command inside a sandbox through the envd connect protocol (the same
# channel the replay framework uses). Returns the command's exit code; output
# lands in EXEC_OUT. XDG_CONFIG_HOME is unset so the CLI reads its credentials
# from the isolated HOME even when the caller has XDG set.
sandbox_exec() { # sandbox_id command... → EXEC_RC, EXEC_OUT, returns command rc
  local id="$1"
  shift
  local out rc
  set +e
  out="$(env -u XDG_CONFIG_HOME -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY -u all_proxy -u ALL_PROXY HOME="${AENV_CLI_HOME}" "${AENV_BIN}" exec "$id" "$@" 2>&1)"
  rc=$?
  set -e
  EXEC_RC="$rc"
  EXEC_OUT="$out"
  return "$rc"
}

sandbox_exec_retry() { # sandbox_id timeout_secs command...
  local id="$1" timeout="$2"
  shift 2
  for ((i = 0; i < timeout * 2; i++)); do
    if sandbox_exec "$id" "$@"; then
      return 0
    fi
    sleep 0.5
  done
  return 1
}

# ── proxy-read (with rebind_in_progress retry) ──────────────────────────────

proxy_read() { # sandbox_id command [cwd] → PR_STATUS, PR_BODY
  local id="$1" command="$2" cwd="${3:-/}"
  local deadline=$((SECONDS + PROXY_READ_RETRY_TIMEOUT))
  while true; do
    api POST "/sandboxes/${id}/proxy-read" \
      "$(jq -nc --arg c "$command" --arg w "$cwd" '{command: $c, cwd: $w}')"
    if [[ "${HTTP_STATUS}" == "503" ]] \
      && [[ "$(jq -r '.code // empty' <<<"${HTTP_BODY}")" == "rebind_in_progress" ]] \
      && ((SECONDS < deadline)); then
      sleep 1
      continue
    fi
    PR_STATUS="${HTTP_STATUS}"
    PR_BODY="${HTTP_BODY}"
    return 0
  done
}

# ════════════════════════════════════════════════════════════════════════════
# Phase 1: server + proxy startup, proxy usability
# ════════════════════════════════════════════════════════════════════════════

section "Phase 1: server startup and resident proxy sandbox"

start_server
pass "server healthy at ${AENV_URL}/health"

note "waiting for the resident proxy sandbox (cold start may pull/convert the image; timeout ${PROXY_START_TIMEOUT}s)"
if wait_for_log_event "resident proxy sandbox started" . "${PROXY_START_TIMEOUT}"; then
  pass "log: resident proxy sandbox started"
else
  fail "log: resident proxy sandbox started" "not found in ${PROXY_START_TIMEOUT}s"
  die "proxy sandbox did not start; aborting"
fi

PROXY_SANDBOX_ID="$(grep -F '"resident proxy sandbox started' "${SERVER_LOG}" 2>/dev/null \
  | jq -r 'select(.fields.message | startswith("resident proxy sandbox started")) | .fields.sandbox_id' 2>/dev/null | head -n1)"
if [[ -z "${PROXY_SANDBOX_ID}" ]]; then
  api GET "/sandboxes"
  PROXY_SANDBOX_ID="$(jq -r '.[0].sandboxID // empty' <<<"${HTTP_BODY}")"
fi
[[ -n "${PROXY_SANDBOX_ID}" ]] || die "could not determine the proxy sandbox id"
note "proxy sandbox id: ${PROXY_SANDBOX_ID}"

if wait_for_sandbox_state "${PROXY_SANDBOX_ID}" "running" 30; then
  pass "proxy sandbox state is running"
else
  fail "proxy sandbox state is running" "$(get_sandbox_state "${PROXY_SANDBOX_ID}")" "running"
fi

if log_has_event "registered proxy sandbox slots"; then
  pass "log: proxy slots registered into the binding table"
else
  fail "log: proxy slots registered into the binding table"
fi

# Clean stale sandboxes persisted by earlier aborted runs (business sandboxes
# persist as Paused; the proxy itself is always recreated fresh).
api GET "/sandboxes"
if [[ "${HTTP_STATUS}" == "200" ]]; then
  while read -r stale_id; do
    [[ -z "${stale_id}" || "${stale_id}" == "${PROXY_SANDBOX_ID}" ]] && continue
    note "deleting stale sandbox from an earlier run: ${stale_id}"
    api DELETE "/sandboxes/${stale_id}"
  done < <(jq -r '.[].sandboxID' <<<"${HTTP_BODY}")
fi

note "executing 'ls /' inside the proxy sandbox"
if sandbox_exec_retry "${PROXY_SANDBOX_ID}" 30 ls /; then
  pass "proxy sandbox can execute commands (aenv exec ls /)"
else
  fail "proxy sandbox can execute commands (aenv exec ls /)" "rc=${EXEC_RC}: ${EXEC_OUT}"
fi
if grep -qx 'etc' <<<"${EXEC_OUT:-}" && grep -qx 'bin' <<<"${EXEC_OUT:-}"; then
  pass "proxy sandbox ls / shows a populated rootfs"
else
  fail "proxy sandbox ls / shows a populated rootfs" "${EXEC_OUT:-}" "etc/bin entries"
fi

note "host ublk devices after proxy boot: $(ublk_device_count)"

# ════════════════════════════════════════════════════════════════════════════
# Phase 2: business sandbox first pause → BIND (ublk device + mount + data)
# ════════════════════════════════════════════════════════════════════════════

section "Phase 2: business sandbox first pause (BIND)"

UBLK_BEFORE_CREATE="$(ublk_device_count)"

api POST "/sandboxes-cold" \
  "$(jq -nc --arg img "${PROXY_E2E_IMAGE}" '{image: $img, timeout: 1800, autoPause: false}')"
if [[ "${HTTP_STATUS}" == "201" ]]; then
  pass "POST /sandboxes-cold returns 201"
else
  fail "POST /sandboxes-cold returns 201" "HTTP ${HTTP_STATUS}: ${HTTP_BODY}"
  die "business sandbox creation failed"
fi
BUSINESS_SANDBOX_ID="$(jq -r '.sandboxID // empty' <<<"${HTTP_BODY}")"
[[ -n "${BUSINESS_SANDBOX_ID}" ]] || die "no sandboxID in create response"
note "business sandbox id: ${BUSINESS_SANDBOX_ID}"

if wait_for_sandbox_state "${BUSINESS_SANDBOX_ID}" "running" "${SANDBOX_READY_TIMEOUT}"; then
  pass "business sandbox is running (cached image, no rebuild)"
else
  fail "business sandbox is running" "$(get_sandbox_state "${BUSINESS_SANDBOX_ID}")" "running"
  die "business sandbox never became running"
fi

MARKER_FILE="/proxy-e2e-marker.txt"
MARKER_GEN1="proxy-e2e-generation-1-$(date +%s)"
if sandbox_exec_retry "${BUSINESS_SANDBOX_ID}" 30 sh -c "echo '${MARKER_GEN1}' > ${MARKER_FILE}"; then
  pass "wrote marker file (gen1) into the business sandbox"
else
  fail "wrote marker file (gen1) into the business sandbox" "rc=${EXEC_RC}: ${EXEC_OUT}"
fi

UBLK_BEFORE_PAUSE="$(ublk_device_count)"
api POST "/sandboxes/${BUSINESS_SANDBOX_ID}/pause"
if [[ "${HTTP_STATUS}" == "204" ]]; then
  pass "POST /pause returns 204"
else
  fail "POST /pause returns 204" "HTTP ${HTTP_STATUS}: ${HTTP_BODY}"
fi
if wait_for_sandbox_state "${BUSINESS_SANDBOX_ID}" "paused" 60; then
  pass "business sandbox state is paused"
else
  fail "business sandbox state is paused" "$(get_sandbox_state "${BUSINESS_SANDBOX_ID}")" "paused"
fi

# BIND: device create → AdoptSlotTarget → guest mount, all inside the async
# binding task; "proxy binding ready" is the end-of-chain anchor.
if wait_for_log_event "proxy binding ready" '.fields.target == "'"${BUSINESS_SANDBOX_ID}"'" and .fields.generation == 1 and .fields.first_bind == true'; then
  pass "log: BIND completed (proxy binding ready, generation=1, first_bind=true)"
else
  fail "log: BIND completed (proxy binding ready, generation=1, first_bind=true)" "timeout ${BIND_TIMEOUT}s"
fi
if log_has_event "proxy_bind_failed"; then
  fail "no proxy_bind_failed in log" "proxy_bind_failed present" "absent"
else
  pass "no proxy_bind_failed in log"
fi

# The staged-config ublk device: slot dev id from the allocation log line.
SLOT_DEV_ID="$(grep -F '"allocated proxy slot for first pause"' "${SERVER_LOG}" 2>/dev/null \
  | jq -r "select(.fields.target == \"${BUSINESS_SANDBOX_ID}\") | .fields.slot_dev_id" 2>/dev/null | head -n1)"
if [[ -n "${SLOT_DEV_ID}" ]] && [[ -e "/dev/ublkb${SLOT_DEV_ID}" ]]; then
  pass "proxy slot ublk device exists on host (/dev/ublkb${SLOT_DEV_ID})"
elif [[ -z "${SLOT_DEV_ID}" ]]; then
  note "slot_dev_id not found in log (debug logging off?); skipping host device check"
else
  fail "proxy slot ublk device exists on host" "/dev/ublkb${SLOT_DEV_ID} missing"
fi
if [[ -f "${DAEMON_LOG}" ]] && grep -q "slot adopted source overlaybd stack" "${DAEMON_LOG}"; then
  pass "daemon log: slot adopted source overlaybd stack"
else
  note "daemon log line 'slot adopted source overlaybd stack' not found (daemon log: ${DAEMON_LOG}; informational)"
fi
if log_has_event "proxy slot mounted" '.fields.target == "'"${BUSINESS_SANDBOX_ID}"'"'; then
  pass "log: slot mounted in proxy guest (ro,noload)"
else
  fail "log: slot mounted in proxy guest (ro,noload)"
fi
note "host ublk devices: before-create=${UBLK_BEFORE_CREATE} before-pause=${UBLK_BEFORE_PAUSE} after-bind=$(ublk_device_count)"

# Direct mount verification inside the proxy guest, independent of proxy-read.
MOUNT_ROOT="/mnt/aenv-proxy-targets"
if sandbox_exec "${PROXY_SANDBOX_ID}" ls "${MOUNT_ROOT}/${BUSINESS_SANDBOX_ID}/"; then
  pass "proxy guest sees the target mount (${MOUNT_ROOT}/${BUSINESS_SANDBOX_ID})"
else
  fail "proxy guest sees the target mount" "rc=${EXEC_RC}: ${EXEC_OUT}"
fi
if sandbox_exec "${PROXY_SANDBOX_ID}" cat "${MOUNT_ROOT}/${BUSINESS_SANDBOX_ID}${MARKER_FILE}"; then
  if [[ "${EXEC_OUT}" == "${MARKER_GEN1}" ]]; then
    pass "proxy guest reads marker (gen1) through the mounted ublk device"
  else
    fail "proxy guest reads marker (gen1) through the mounted ublk device" "${EXEC_OUT}" "${MARKER_GEN1}"
  fi
else
  fail "proxy guest reads marker (gen1) through the mounted ublk device" "rc=${EXEC_RC}: ${EXEC_OUT}"
fi

# ════════════════════════════════════════════════════════════════════════════
# Phase 3: proxy-read correctness
# ════════════════════════════════════════════════════════════════════════════

section "Phase 3: proxy-read on the paused target"

proxy_read "${BUSINESS_SANDBOX_ID}" "cat ${MARKER_FILE}" "/"
if [[ "${PR_STATUS}" == "200" ]]; then
  pass "proxy-read returns 200"
else
  fail "proxy-read returns 200" "HTTP ${PR_STATUS}: ${PR_BODY}"
fi
EXIT_CODE="$(jq -r '.exitCode // empty' <<<"${PR_BODY}")"
STDOUT="$(jq -r '.stdout // empty' <<<"${PR_BODY}")"
GENERATION="$(jq -r '.bindingGeneration // empty' <<<"${PR_BODY}")"
if [[ "${EXIT_CODE}" == "0" ]]; then
  pass "proxy-read command exit code is 0"
else
  fail "proxy-read command exit code is 0" "${EXIT_CODE}" "0"
fi
if [[ "${STDOUT}" == "${MARKER_GEN1}" ]]; then
  pass "proxy-read returns the gen1 marker content"
else
  fail "proxy-read returns the gen1 marker content" "${STDOUT}" "${MARKER_GEN1}"
fi
if [[ "${GENERATION}" == "1" ]]; then
  pass "proxy-read reports bindingGeneration=1"
else
  fail "proxy-read reports bindingGeneration=1" "${GENERATION}" "1"
fi

# ── Phase 4: resume, dirty writes, pause → REBIND ──────────────────────────

section "Phase 4: resume, dirty-page writes, second pause (REBIND)"

api POST "/sandboxes/${BUSINESS_SANDBOX_ID}/connect" '{"timeout":1800}'
if [[ "${HTTP_STATUS}" == "201" || "${HTTP_STATUS}" == "200" ]]; then
  pass "POST /connect resumes the sandbox"
else
  fail "POST /connect resumes the sandbox" "HTTP ${HTTP_STATUS}: ${HTTP_BODY}"
fi
if wait_for_sandbox_state "${BUSINESS_SANDBOX_ID}" "running" 60; then
  pass "business sandbox is running again"
else
  fail "business sandbox is running again" "$(get_sandbox_state "${BUSINESS_SANDBOX_ID}")" "running"
  die "resume failed; cannot continue"
fi

# Contract check: a running target must be rejected, never silently served
# from the stale generation-1 binding.
proxy_read "${BUSINESS_SANDBOX_ID}" "cat ${MARKER_FILE}" "/"
if [[ "${PR_STATUS}" == "409" ]] \
  && [[ "$(jq -r '.code // empty' <<<"${PR_BODY}")" == "target_not_paused" ]]; then
  pass "proxy-read on a running target returns 409 target_not_paused"
else
  fail "proxy-read on a running target returns 409 target_not_paused" \
    "HTTP ${PR_STATUS}: ${PR_BODY}" "409 target_not_paused"
fi

MARKER_GEN2="proxy-e2e-generation-2-$(date +%s)"
# Write actions with NO explicit sync: pause-time rootfs sync (design change
# #2) must flush these dirty pages into the sealed snapshot. dd failures are
# tolerated here on purpose; phase 5 asserts the dirty file's actual size.
if sandbox_exec_retry "${BUSINESS_SANDBOX_ID}" 30 sh -c \
  "echo '${MARKER_GEN2}' > ${MARKER_FILE}; dd if=/dev/zero of=/proxy-e2e-dirty.bin bs=1M count=${DIRTY_WRITE_MB} 2>/dev/null || true"; then
  pass "wrote marker (gen2) + ${DIRTY_WRITE_MB}MB dirty data without sync"
else
  fail "wrote marker (gen2) + ${DIRTY_WRITE_MB}MB dirty data without sync" "rc=${EXEC_RC}: ${EXEC_OUT}"
fi

api POST "/sandboxes/${BUSINESS_SANDBOX_ID}/pause"
if [[ "${HTTP_STATUS}" == "204" ]]; then
  pass "second POST /pause returns 204"
else
  fail "second POST /pause returns 204" "HTTP ${HTTP_STATUS}: ${HTTP_BODY}"
fi
if wait_for_sandbox_state "${BUSINESS_SANDBOX_ID}" "paused" 60; then
  pass "business sandbox is paused again"
else
  fail "business sandbox is paused again" "$(get_sandbox_state "${BUSINESS_SANDBOX_ID}")" "paused"
fi

if wait_for_log_event "proxy binding ready" '.fields.target == "'"${BUSINESS_SANDBOX_ID}"'" and .fields.generation == 2 and .fields.first_bind == false'; then
  pass "log: REBIND completed (proxy binding ready, generation=2, first_bind=false)"
else
  fail "log: REBIND completed (proxy binding ready, generation=2, first_bind=false)" "timeout ${BIND_TIMEOUT}s"
fi

# ── Phase 5: read the freshly written (dirty-page) content ─────────────────

section "Phase 5: proxy-read observes the new generation"

proxy_read "${BUSINESS_SANDBOX_ID}" "cat ${MARKER_FILE}" "/"
if [[ "${PR_STATUS}" == "200" ]]; then
  pass "proxy-read (gen2) returns 200"
else
  fail "proxy-read (gen2) returns 200" "HTTP ${PR_STATUS}: ${PR_BODY}"
fi
EXIT_CODE="$(jq -r '.exitCode // empty' <<<"${PR_BODY}")"
STDOUT="$(jq -r '.stdout // empty' <<<"${PR_BODY}")"
GENERATION="$(jq -r '.bindingGeneration // empty' <<<"${PR_BODY}")"
if [[ "${EXIT_CODE}" == "0" ]]; then
  pass "proxy-read (gen2) command exit code is 0"
else
  fail "proxy-read (gen2) command exit code is 0" "${EXIT_CODE}" "0"
fi
if [[ "${STDOUT}" == "${MARKER_GEN2}" ]]; then
  pass "proxy-read returns the gen2 marker (dirty pages flushed + rebind visible)"
else
  fail "proxy-read returns the gen2 marker (dirty pages flushed + rebind visible)" "${STDOUT}" "${MARKER_GEN2}"
fi
if [[ "${GENERATION}" == "2" ]]; then
  pass "proxy-read reports bindingGeneration=2"
else
  fail "proxy-read reports bindingGeneration=2" "${GENERATION}" "2"
fi
if [[ "${STDOUT}" != "${MARKER_GEN1}" ]]; then
  pass "gen2 read is not the stale gen1 content"
else
  fail "gen2 read is not the stale gen1 content" "${STDOUT}" "different from ${MARKER_GEN1}"
fi

# Cross-check the dirty-page file made it into the new generation.
proxy_read "${BUSINESS_SANDBOX_ID}" "wc -c < /proxy-e2e-dirty.bin" "/"
DIRTY_SIZE="$(jq -r '.stdout // empty' <<<"${PR_BODY}" | tr -d '[:space:]')"
WANT_DIRTY=$((DIRTY_WRITE_MB * 1024 * 1024))
if [[ "${DIRTY_SIZE}" == "${WANT_DIRTY}" ]]; then
  pass "dirty data file size correct (${DIRTY_SIZE} bytes)"
else
  fail "dirty data file size correct" "${DIRTY_SIZE:-none}" "${WANT_DIRTY}"
fi

# Also verify through the direct mount path (drop_caches made the new stack
# visible to the persistent guest mount).
if sandbox_exec "${PROXY_SANDBOX_ID}" cat "${MOUNT_ROOT}/${BUSINESS_SANDBOX_ID}${MARKER_FILE}"; then
  if [[ "${EXEC_OUT}" == "${MARKER_GEN2}" ]]; then
    pass "proxy guest direct mount read shows gen2 content (rebind + drop_caches)"
  else
    fail "proxy guest direct mount read shows gen2 content" "${EXEC_OUT}" "${MARKER_GEN2}"
  fi
else
  fail "proxy guest direct mount read shows gen2 content" "rc=${EXEC_RC}: ${EXEC_OUT}"
fi

# ── Cleanup: delete must release the binding (UNBIND) ───────────────────────

section "Cleanup: delete business sandbox (UNBIND)"

api DELETE "/sandboxes/${BUSINESS_SANDBOX_ID}"
if [[ "${HTTP_STATUS}" == "204" ]]; then
  pass "DELETE /sandboxes returns 204"
else
  fail "DELETE /sandboxes returns 204" "HTTP ${HTTP_STATUS}: ${HTTP_BODY}"
fi
if wait_for_log_event "proxy binding released" '.fields.target == "'"${BUSINESS_SANDBOX_ID}"'"'; then
  pass "log: proxy binding released (UNBIND before artifact cleanup)"
else
  fail "log: proxy binding released (UNBIND before artifact cleanup)" "timeout ${BIND_TIMEOUT}s"
fi
BUSINESS_SANDBOX_ID=""

# ════════════════════════════════════════════════════════════════════════════
# Summary
# ════════════════════════════════════════════════════════════════════════════

section "Summary"
printf '  passed: %d\n  failed: %d\n' "${PASS_COUNT}" "${FAIL_COUNT}"
printf '  artifacts: server log %s\n             daemon log %s\n             aenv home %s\n' \
  "${SERVER_LOG}" "${DAEMON_LOG}" "${AENV_HOME_PATH}"

if ((FAIL_COUNT > 0)); then
  exit 1
fi
printf '  ALL CHECKS PASSED\n'
exit 0
