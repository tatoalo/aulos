#!/usr/bin/env bash
# WP-00 — regenerate every golden corpus in one shot.
#
# Runs the two pure-Python dumps, then starts the legacy server twice (once
# against an empty STATE_DIR, once against a seeded one) and drives the v1
# capture against each. Nothing here reaches the network: the legacy server is
# started with MAX_CONCURRENT_DOWNLOADS=0 and every captured route answers
# before yt-dlp is constructed.
#
# Usage:
#   tools/capture/run_capture.sh
#   METUBE_POT_ROOT=/path/to/metube_pot tools/capture/run_capture.sh
#
# Prerequisites in the legacy checkout:
#   uv sync --frozen --group dev      (creates .venv)

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
LEGACY_ROOT="${METUBE_POT_ROOT:-/Users/apogliaghi/Development/metube_pot}"
PY="${LEGACY_ROOT}/.venv/bin/python"
PORT="${CAPTURE_PORT:-18081}"
BASE_URL="http://127.0.0.1:${PORT}"

# The legacy static route needs ui/dist/metube/browser to exist (aiohttp's
# static handler refuses a missing directory). We create a placeholder when the
# Angular build is absent and remove it again on exit — the legacy checkout must
# be left byte-clean, and `pnpm build` is explicitly not run.
UI_DIR="${LEGACY_ROOT}/ui/dist/metube/browser"
UI_PLACEHOLDER=0

TMP_ROOT=""
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  if [[ -n "${TMP_ROOT}" && -d "${TMP_ROOT}" ]]; then
    rm -rf "${TMP_ROOT}"
  fi
  if [[ "${UI_PLACEHOLDER}" == "1" ]]; then
    rm -rf "${LEGACY_ROOT}/ui/dist"
  fi
}
trap cleanup EXIT

if [[ ! -x "${PY}" ]]; then
  echo "legacy venv not found at ${PY}" >&2
  echo "run: cd ${LEGACY_ROOT} && uv sync --frozen --group dev" >&2
  exit 1
fi

if [[ ! -f "${UI_DIR}/index.html" ]]; then
  mkdir -p "${UI_DIR}"
  : > "${UI_DIR}/index.html"
  UI_PLACEHOLDER=1
  echo "created a placeholder ${UI_DIR}/index.html (removed on exit)"
fi

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aulos-wp00.XXXXXX")"
echo "scratch: ${TMP_ROOT}"

# A stray legacy server on the capture port would be silently load-balanced
# with ours (aiohttp uses SO_REUSEPORT), so refuse to start.
if "${PY}" - "${PORT}" <<'EOF'
import socket, sys
s = socket.socket()
s.settimeout(0.5)
sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
EOF
then
  echo "something is already listening on 127.0.0.1:${PORT}; stop it or set CAPTURE_PORT" >&2
  exit 1
fi

YTDLP_VERSION="$("${PY}" -c 'import yt_dlp.version; print(yt_dlp.version.__version__)')"

# ---------------------------------------------------------------------------
# 1. the pure-Python dumps
# ---------------------------------------------------------------------------
echo
echo "== tests/golden =="
( cd "${HERE}" && METUBE_POT_ROOT="${LEGACY_ROOT}" "${PY}" dump_formats.py )
( cd "${HERE}" && METUBE_POT_ROOT="${LEGACY_ROOT}" "${PY}" dump_progress_vectors.py )

# ---------------------------------------------------------------------------
# 2. the v1 corpus
# ---------------------------------------------------------------------------
# Every knob that shapes a captured response is set explicitly, and the same
# list is echoed into MANIFEST.json so the corpus can be re-derived.
#
# MAX_CONCURRENT_DOWNLOADS=0 is load-bearing: DownloadQueue.initialize()
# auto-restarts everything in queue.json, and a zero-permit semaphore parks
# those tasks forever, so the seeded statuses survive and nothing dials out.
SERVER_ENV=()
build_server_env() {
  local root="$1" state="$2"
  SERVER_ENV=(
    "DOWNLOAD_DIR=${root}/downloads"
    "AUDIO_DOWNLOAD_DIR=${root}/downloads"
    "TEMP_DIR=${root}/tmp"
    "STATE_DIR=${state}"
    "PORT=${PORT}"
    "HOST=127.0.0.1"
    "URL_PREFIX="
    "LOGLEVEL=INFO"
    "MAX_CONCURRENT_DOWNLOADS=0"
    "CUSTOM_DIRS=true"
    "CREATE_CUSTOM_DIRS=true"
    "DELETE_FILE_ON_TRASHCAN=false"
    "ALLOW_YTDL_OPTIONS_OVERRIDES=false"
    "CORS_ALLOWED_ORIGINS=https://ui.example.com"
    'YTDL_OPTIONS_PRESETS={"archive": {"writesubtitles": true}, "fast": {"concurrent_fragment_downloads": 4}}'
    "TELEGRAM_BOT_ENABLED=false"
    "JELLYFIN_SYNC_ENABLED=false"
    "DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT=0"
    "SUBSCRIPTION_DEFAULT_CHECK_INTERVAL=60"
    "METUBE_VERSION=wp00-capture"
  )
}

port_is_open() {
  "${PY}" - "$PORT" <<'EOF'
import socket, sys
s = socket.socket()
s.settimeout(0.5)
sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)
EOF
}

start_server() {
  local root="$1" state="$2" log="$3"
  mkdir -p "${root}/downloads" "${root}/tmp" "${state}"
  # `exec` matters: without it $! is the subshell and the kill below leaves the
  # Python process alive. aiohttp binds with SO_REUSEPORT, so a survivor does
  # not even fail to start the next phase — the kernel silently load-balances
  # between the two servers and the capture records a mix of both states.
  ( cd "${LEGACY_ROOT}" && exec env "${SERVER_ENV[@]}" "${PY}" app/main.py >"${log}" 2>&1 ) &
  SERVER_PID=$!
  echo "started legacy server pid ${SERVER_PID}, log ${log}"
}

stop_server() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  SERVER_PID=""
  local i
  for i in $(seq 1 40); do
    if ! port_is_open; then
      return 0
    fi
    sleep 0.25
  done
  echo "port ${PORT} is still accepting connections after the server was killed" >&2
  exit 1
}

run_phase() {
  local phase="$1" root="$2" state="$3"
  echo
  echo "== tests/v1_golden (phase: ${phase}) =="
  build_server_env "${root}" "${state}"
  start_server "${root}" "${state}" "${TMP_ROOT}/server-${phase}.log"
  local env_text
  env_text="$(printf '%s\n' "${SERVER_ENV[@]}")"
  set +e
  ( cd "${HERE}" \
      && METUBE_POT_ROOT="${LEGACY_ROOT}" \
         CAPTURE_YTDLP_VERSION="${YTDLP_VERSION}" \
         CAPTURE_SERVER_ENV="${env_text}" \
         "${PY}" capture_v1.py --base-url "${BASE_URL}" --phase "${phase}" \
             --state-dir "${state}" --scratch-root "${TMP_ROOT}" )
  local rc=$?
  set -e
  stop_server
  if [[ ${rc} -ne 0 ]]; then
    echo "capture phase ${phase} failed; server log:" >&2
    tail -40 "${TMP_ROOT}/server-${phase}.log" >&2
    exit ${rc}
  fi
}

# Phase 1: a pristine STATE_DIR, for the empty-history case.
run_phase empty "${TMP_ROOT}/empty" "${TMP_ROOT}/empty/.metube"

# Phase 2: the seeded STATE_DIR, for everything else.
SEED_ROOT="${TMP_ROOT}/seeded"
SEED_STATE="${SEED_ROOT}/.metube"
mkdir -p "${SEED_STATE}"
"${PY}" "${HERE}/seed_state.py" "${SEED_STATE}"
run_phase seeded "${SEED_ROOT}" "${SEED_STATE}"

# ---------------------------------------------------------------------------
# 3. verify
# ---------------------------------------------------------------------------
echo
echo "== verify =="
"${PY}" "${HERE}/verify.py"

echo
echo "done. corpora under ${REPO_ROOT}/tests/"
