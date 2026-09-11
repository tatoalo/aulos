#!/usr/bin/env bash
# End-to-end acceptance for the shipped image (BRIEF §17, PLAN WP-17).
#
# It runs the REAL container against a REAL public Creative-Commons video, over a temporary volume,
# and asserts the things only a container can be asked:
#
#   1. `healthz` is green, `pot` included -- the sidecar is supervised, which legacy could not see
#   2. `POST api/v2/downloads` for a small CC video answers 202 before any extraction
#   3. the WebSocket carries `added -> delta -> completed` for that id
#   4. the produced file is in the volume with the entrypoint's PUID/PGID/UMASK applied, and
#      `GET download/<name>` honours `Range`
#   5. the v1 shim's `POST add` works and `GET history` has all three keys
#   6. `socket.io` answers 501 rather than pretending
#   7. a restart mid-download resumes rather than stranding the item
#   8. `docker logs` contains no ERROR
#   9. a second profile seeds a legacy STATE_DIR and asserts the import report has zero errors,
#      does not re-import on the next boot, runs with CHOWN_DIRS=false and UMASK=077, and hands
#      every root it created (including a split AUDIO_DOWNLOAD_DIR) to PUID:PGID
#
# Gated on AULOS_E2E=1 so a plain `cargo test` / `./run.sh` never reaches for the network.
#
#   AULOS_E2E=1 tests/e2e/run.sh
#
# Knobs:
#   AULOS_IMAGE        image to test          (default aulos-server:dev)
#   AULOS_E2E_BUILD    1 = build it first     (default 0)
#   AULOS_E2E_URL      the video to download  (default the Blender Foundation's CC-BY trailer)
#   AULOS_E2E_KEEP     1 = keep the container and volume for inspection
#   AULOS_E2E_PORT     host port              (default 18081)
#   AULOS_E2E_PLATFORM docker platform        (default: the host's, i.e. no --platform)
#
# The release architecture is linux/amd64 (BRIEF §16). To exercise *that* image on an arm64
# Mac, where OrbStack runs amd64 under Rosetta:
#
#   AULOS_E2E=1 AULOS_E2E_PLATFORM=linux/amd64 AULOS_E2E_BUILD=1 tests/e2e/run.sh
set -euo pipefail

if [ "${AULOS_E2E:-0}" != "1" ]; then
  echo "AULOS_E2E is not 1; skipping the end-to-end suite"
  exit 0
fi

IMAGE="${AULOS_IMAGE:-aulos-server:dev}"
PORT="${AULOS_E2E_PORT:-18081}"
# "Big Buck Bunny" trailer, Blender Foundation, CC-BY: small, stable, and legal to fetch in CI.
VIDEO="${AULOS_E2E_URL:-https://www.youtube.com/watch?v=aqz-KE-bpKQ}"
BASE="http://127.0.0.1:${PORT}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "${HERE}/../.." && pwd)"
NAME="aulos-e2e-$$"
# `--platform` is passed only when the caller asked for one, so the default stays the host's
# native platform and nothing is emulated unless that was the point. The `+` expansion keeps
# an empty array legal under `set -u` on every bash this script might meet.
PLATFORM_ARGS=()
if [ -n "${AULOS_E2E_PLATFORM:-}" ]; then
  PLATFORM_ARGS=(--platform "${AULOS_E2E_PLATFORM}")
fi
VOLUME="aulos-e2e-vol-$$"
FAILED=0

log()  { printf '\n=== %s\n' "$*"; }
ok()   { printf '  ok   %s\n' "$*"; }
fail() { printf '  FAIL %s\n' "$*" >&2; FAILED=1; }
die()  { printf '\nFATAL %s\n' "$*" >&2; exit 1; }

cleanup() {
  local code=$?
  if [ "${AULOS_E2E_KEEP:-0}" = "1" ]; then
    printf '\nAULOS_E2E_KEEP=1: leaving container %s and volume %s\n' "$NAME" "$VOLUME"
    return
  fi
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker volume rm -f "$VOLUME" >/dev/null 2>&1 || true
  exit "$code"
}
trap cleanup EXIT INT TERM

# --- helpers ---------------------------------------------------------------------------------

# `curl` with a short timeout, printing the body.
req() { curl -sS --max-time 30 "$@"; }

# The HTTP status of a request.
code() { curl -sS -o /dev/null -w '%{http_code}' --max-time 30 "$@"; }

# One JSON field, without needing `jq` on the host.
jget() { python3 -c '
import json,sys
doc = json.load(sys.stdin)
for key in sys.argv[1].split("."):
    if key == "":
        continue
    if isinstance(doc, list):
        doc = doc[int(key)]
    else:
        doc = doc.get(key)
    if doc is None:
        break
print("" if doc is None else (doc if isinstance(doc, str) else json.dumps(doc)))
' "$1"; }

# Waits until `f` prints something non-empty, or fails after N seconds.
wait_for() {
  local what="$1" secs="$2"; shift 2
  local deadline=$(( $(date +%s) + secs ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if out="$("$@" 2>/dev/null)" && [ -n "$out" ]; then
      printf '%s' "$out"
      return 0
    fi
    sleep 1
  done
  # The label the caller passed is what makes a timeout readable in the log; every caller already
  # reports its own failure, so this is a note on stderr rather than a `fail`.
  printf '  ...  gave up waiting for %s after %ss\n' "$what" "$secs" >&2
  return 1
}

item_status() {
  req "${BASE}/api/v2/items/$1" | jget status
}

# --- 0. the image ----------------------------------------------------------------------------

log "image ${IMAGE}"
if [ "${AULOS_E2E_BUILD:-0}" = "1" ]; then
  docker build ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
    -f "${ROOT}/docker/Dockerfile" -t "$IMAGE" "$ROOT" \
    || die "docker build failed"
fi
docker image inspect "$IMAGE" >/dev/null 2>&1 \
  || die "no such image: $IMAGE (build it, or set AULOS_IMAGE / AULOS_E2E_BUILD=1)"
ok "image is present"

# `doctor` must be green inside the image: the pinned yt-dlp, ffmpeg, deno and N_m3u8DL-RE are all
# things the Dockerfile installs, and a missing one is a packaging bug rather than a runtime one.
docker run --rm ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} "$IMAGE" doctor \
  || die "doctor failed inside the image"
ok "doctor: every required tool is present"

# --- 1. profile A: a fresh volume -------------------------------------------------------------

log "profile A: a fresh volume"
docker volume create "$VOLUME" >/dev/null
docker run -d --name "$NAME" ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
  -p "127.0.0.1:${PORT}:8081" \
  -v "${VOLUME}:/downloads" \
  -e PUID=1000 -e PGID=1000 -e UMASK=022 \
  -e AULOS_E2E=1 \
  -e LOGLEVEL=INFO \
  -e AULOS_WS_BATCH_MS=250 \
  -e TELEGRAM_BOT_ENABLED=false \
  -e AULOS_NFO_ENABLED=true \
  -e 'YTDL_OPTIONS={"writeinfojson": true}' \
  "$IMAGE" >/dev/null || die "docker run failed"
ok "container started"

# The container's own HEALTHCHECK is the `healthcheck` subcommand, so waiting for `healthy` also
# proves the URL_PREFIX normalisation path works in the image.
health=""
for _ in $(seq 1 60); do
  health="$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo starting)"
  [ "$health" = "healthy" ] && break
  [ "$health" = "unhealthy" ] && break
  sleep 2
done
[ "$health" = "healthy" ] || {
  docker logs "$NAME" | tail -60
  die "the container never became healthy (last: $health)"
}
ok "the container's HEALTHCHECK reports healthy"

# --- 2. healthz, pot included -----------------------------------------------------------------

log "healthz"
http="$(code "${BASE}/healthz")"
body="$(req "${BASE}/healthz")" || die "healthz did not answer"
status="$(printf '%s' "$body" | jget status)"
# DESIGN §16.3: `503` **only** when the store is unusable or the WAL is over 256 MB. The body's
# word is a roll-up over every component, so a single `down` optional component can make it say
# `down` under a `200`; the HTTP status is the verdict, as it is for the `healthcheck` subcommand.
[ "$http" = "200" ] && ok "healthz → 200 (status=$status)" || {
  printf '%s\n' "$body"
  fail "healthz → $http"
}
case "$status" in
  ok|degraded) ok "the roll-up is $status" ;;
  *) printf '%s\n' "$body" | head -40
     fail "the roll-up is $status; a component is down inside the image" ;;
esac

pot="$(printf '%s' "$body" | jget components.pot.status)"
# The sidecar is supervised, so it is `ok` once its probe has answered and `degraded` in the first
# fifteen seconds. `down`/`failed` means the supervisor could not keep it alive, which is exactly
# the legacy blind spot this component exists to expose.
case "$pot" in
  ok|degraded)
    ok "components.pot=$pot (pid $(printf '%s' "$body" | jget components.pot.pid))" ;;
  *)
    printf '%s\n' "$body" | head -40
    fail "components.pot=$pot -- the bgutil-pot sidecar is not being supervised" ;;
esac

for key in store queue ytdlp_runner ffmpeg nm3u8dl deno ytdl_options importer telegram \
           jellyfin nfo audio_sync events subscriptions; do
  have="$(printf '%s' "$body" | jget "components.${key}.status")"
  [ -n "$have" ] || fail "healthz has no components.${key}"
done
ok "every DESIGN §16.3 component is present"

ytdlp="$(printf '%s' "$body" | jget yt_dlp)"
[ -n "$ytdlp" ] && [ "$ytdlp" != "null" ] \
  && ok "yt_dlp=$ytdlp" \
  || fail "healthz.yt_dlp is null -- the shim identity did not reach ServerInfo"

[ "$(code "${BASE}/livez")" = "200" ] && ok "livez answers 200" || fail "livez"

# --- 3. a v2 add, and the WebSocket sequence ---------------------------------------------------

log "v2 add: ${VIDEO}"
# The socket is opened **before** the add, so the `added` frame cannot be missed: a socket that
# connects afterwards is handed a `snapshot` instead, which is correct protocol behaviour and a
# weaker assertion than the one the acceptance list asks for.
ws_log="$(mktemp)"
python3 "${HERE}/ws_watch.py" \
    --url "ws://127.0.0.1:${PORT}/ws" \
    --match "$(printf '%s' "$VIDEO" | sed 's|^https\{0,1\}://||')" \
    --expect added,delta,completed \
    --timeout 900 > "$ws_log" 2>&1 &
ws_pid=$!
for _ in $(seq 1 100); do
  grep -q '^READY' "$ws_log" && break
  sleep 0.2
done
grep -q '^READY' "$ws_log" || { cat "$ws_log"; die "the WebSocket did not connect"; }
ok "the WebSocket is connected"

add_body="$(req -X POST "${BASE}/api/v2/downloads" \
  -H 'content-type: application/json' \
  -d "{\"url\":\"${VIDEO}\",\"download_type\":\"video\",\"format\":\"mp4\",\"quality\":\"worst\"}")" \
  || die "POST api/v2/downloads failed"
id="$(printf '%s' "$add_body" | jget id)"
[ -n "$id" ] || { printf '%s\n' "$add_body"; die "the add returned no id"; }
ok "202 id=$id"

if wait "$ws_pid"; then
  ok "the WebSocket carried added -> delta -> completed"
else
  tail -30 "$ws_log"
  fail "the added -> delta -> completed sequence did not arrive"
fi
rm -f "$ws_log"

final="$(item_status "$id")"
[ "$final" = "finished" ] && ok "the item finished" || fail "the item ended $final"

# --- 4. the file, in the volume, over the file route ------------------------------------------

log "the produced file"
item="$(req "${BASE}/api/v2/items/${id}")"
filename="$(printf '%s' "$item" | jget filename)"
download_url="$(printf '%s' "$item" | jget download_url)"
size="$(printf '%s' "$item" | jget size)"
[ -n "$filename" ] || { printf '%s\n' "$item"; fail "the item carries no filename"; }
ok "filename=$filename size=$size"

# In the volume, owned by PUID:PGID.
# `test -f` through `docker exec`'s argv, not a shell string: a video title may contain a quote.
if docker exec "$NAME" test -f "/downloads/${filename}"; then
  owner="$(docker exec "$NAME" stat -c '%u:%g' "/downloads/${filename}")"
  ok "the file is in the volume, owned by ${owner}"
  [ "$owner" = "1000:1000" ] || fail "PUID/PGID were not applied (owner=$owner)"
  # The entrypoint's `umask ${UMASK}` is inherited all the way down to yt-dlp, so the produced
  # file's mode is the only place the knob is observable. It cannot be read back with
  # `docker exec … umask`: `exec` does not go through the entrypoint, so that reports the daemon's
  # own 0022 whatever UMASK is set to. Profile B asserts the other direction (UMASK=077 → 600).
  mode="$(docker exec "$NAME" stat -c '%a' "/downloads/${filename}")"
  [ "$mode" = "644" ] \
    && ok "UMASK=022 reached the download (mode $mode)" \
    || fail "UMASK=022 should make the file 644, got $mode"
else
  docker exec "$NAME" ls -la /downloads || true
  fail "the file is not in the volume"
fi

# --- 4b. what the download left beside the file, and how the row reads afterwards -------------
# Production bug round 3 (2026-09-05): the .info.json used to land in DOWNLOAD_DIR while the media
# was still in the per-job scratch dir, the built-in NFO hook only applied to StreamingCommunity,
# and a finished row kept yt-dlp's last postprocessor line ("MoveFiles…") as its msg.

log "sidecars and the terminal row"
stem="${filename%.*}"
# Since ef5e947 the NFO hook consumes the .info.json and always deletes it once the .nfo is
# written, so with the hook on the sidecar must be gone, not beside the media.
if docker exec "$NAME" test -f "/downloads/${stem}.info.json"; then
  docker exec "$NAME" ls -la /downloads || true
  fail "the .info.json survived (the NFO hook deletes it after writing the .nfo)"
else
  ok "the .info.json was consumed by the NFO hook"
fi
if docker exec "$NAME" test -f "/downloads/${stem}.nfo"; then
  ok "the built-in NFO hook wrote ${stem}.nfo"
  docker exec "$NAME" grep -q '<uniqueid type="youtube"' "/downloads/${stem}.nfo" \
    && ok "the NFO carries the youtube uniqueid" \
    || fail "the NFO has no <uniqueid type=\"youtube\">"
else
  fail "no .nfo beside the media: the NFO hook did not run for a yt-dlp item"
fi
[ "$(docker exec "$NAME" sh -c 'ls -d /downloads/0*/ 2>/dev/null | wc -l' | tr -d ' ')" = "0" ] \
  && ok "no per-job scratch directory survived the download" \
  || fail "a per-job scratch directory is still under /downloads"
term_msg="$(printf '%s' "$item" | jget msg)"
[ -z "$term_msg" ] \
  && ok "the finished row carries msg=null" \
  || fail "the finished row still carries msg=${term_msg}"
v1_msg="$(req "${BASE}/history" | jget done.0.msg)"
[ -z "$v1_msg" ] \
  && ok "v1 history: done[0].msg is null" \
  || fail "v1 history: done[0].msg=${v1_msg}"
# `healthz` is rate-limited and answers a cached component set when polled again too soon (the
# suite called it a moment ago), and the hooks run after the terminal write — so poll for a while.
nfo_runs=0
for _ in $(seq 1 30); do
  nfo_runs="$(req "${BASE}/healthz" | jget components.nfo.runs_total)"
  [ "${nfo_runs:-0}" -ge 1 ] 2>/dev/null && break
  sleep 1
done
[ "${nfo_runs:-0}" -ge 1 ] 2>/dev/null \
  && ok "healthz: components.nfo.runs_total=${nfo_runs}" \
  || fail "healthz: components.nfo.runs_total=${nfo_runs:-<absent>} 30 s after a completed download"

# The file route, then the same route with a Range.
full_code="$(code "${BASE}/${download_url}")"
[ "$full_code" = "200" ] && ok "GET ${download_url} → 200" || fail "GET ${download_url} → $full_code"

range_head="$(curl -sS -D - -o /tmp/aulos-e2e-range.$$ --max-time 30 \
  -H 'Range: bytes=0-99' "${BASE}/${download_url}")"
range_code="$(printf '%s' "$range_head" | head -1 | awk '{print $2}')"
range_len="$(wc -c < /tmp/aulos-e2e-range.$$ | tr -d ' ')"
rm -f /tmp/aulos-e2e-range.$$
if [ "$range_code" = "206" ] && [ "$range_len" = "100" ]; then
  ok "Range: bytes=0-99 → 206 with 100 bytes"
else
  printf '%s\n' "$range_head" | head -8
  fail "Range → $range_code with $range_len bytes (want 206 / 100)"
fi
printf '%s' "$range_head" | grep -qi '^content-range: bytes 0-99/' \
  && ok "Content-Range is present" \
  || fail "Content-Range is missing"

# --- 5. the v1 shim ---------------------------------------------------------------------------

log "the v1 shim"
v1_code="$(code -X POST "${BASE}/add" -H 'content-type: application/json' \
  -d "{\"url\":\"${VIDEO}\",\"quality\":\"worst\",\"format\":\"mp4\"}")"
[ "$v1_code" = "200" ] && ok "POST add → 200" || fail "POST add → $v1_code"

history="$(req "${BASE}/history")"
for key in queue pending "done"; do
  printf '%s' "$history" | python3 -c "
import json,sys
doc = json.load(sys.stdin)
assert '$key' in doc, 'history has no $key'
assert isinstance(doc['$key'], list), '$key is not an array'
" || fail "history.$key"
done
ok "GET history carries queue, pending and done"

[ "$(code "${BASE}/version")" = "200" ] && ok "GET version → 200" || fail "GET version"

# The web UI (DESIGN §24): a browser gets the page, everything else keeps the identity document.
ui_hdrs="$(curl -sS -D - -o /dev/null --max-time 30 -H 'Accept: text/html' "${BASE}/")"
printf '%s' "$ui_hdrs" | grep -qi '^content-type: text/html' \
  && ok "GET / with Accept: text/html → the web UI" \
  || fail "GET / with Accept: text/html did not answer text/html"
printf '%s' "$ui_hdrs" | grep -qi '^content-security-policy: ' \
  && ok "the page carries a Content-Security-Policy" \
  || fail "the page has no Content-Security-Policy header"
[ "$(req -H 'Accept: application/json' "${BASE}/" | jget name)" = "aulos-server" ] \
  && ok "GET / with Accept: application/json → the identity document" \
  || fail "GET / with Accept: application/json is not the identity document"
[ "$(code "${BASE}/assets/app.js")" = "200" ] && ok "GET assets/app.js → 200" || fail "GET assets/app.js"
[ "$(code "${BASE}/manifest.webmanifest")" = "200" ] && ok "GET manifest.webmanifest → 200" || fail "GET manifest.webmanifest"

# --- 6. socket.io is honestly gone ------------------------------------------------------------

log "socket.io"
sio="$(code "${BASE}/socket.io/")"
[ "$sio" = "501" ] && ok "socket.io/ → 501" || fail "socket.io/ → $sio (want 501)"

# --- 7. a restart mid-download resumes ---------------------------------------------------------

log "restart mid-download"
restart_body="$(req -X POST "${BASE}/api/v2/downloads" \
  -H 'content-type: application/json' \
  -d "{\"url\":\"${VIDEO}\",\"download_type\":\"video\",\"format\":\"any\",\"quality\":\"best\"}")"
rid="$(printf '%s' "$restart_body" | jget id)"
[ -n "$rid" ] || die "the restart-profile add returned no id"

running="$(wait_for "the item to start downloading" 180 sh -c "
  s=\$(curl -sS --max-time 10 '${BASE}/api/v2/items/${rid}' \
      | python3 -c 'import json,sys; print(json.load(sys.stdin).get(\"status\",\"\"))')
  [ \"\$s\" = downloading ] && echo \"\$s\"
")" || { docker logs "$NAME" | tail -40; die "the second item never started downloading"; }
ok "the item is $running; restarting the container"

docker restart -t 25 "$NAME" >/dev/null || die "docker restart failed"
health=""
for _ in $(seq 1 60); do
  health="$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo starting)"
  [ "$health" = "healthy" ] && break
  sleep 2
done
[ "$health" = "healthy" ] || { docker logs "$NAME" | tail -60; die "unhealthy after the restart"; }

resumed="$(wait_for "the item to be resumed" 120 sh -c "
  s=\$(curl -sS --max-time 10 '${BASE}/api/v2/items/${rid}' \
      | python3 -c 'import json,sys; print(json.load(sys.stdin).get(\"status\",\"\"))')
  case \"\$s\" in queued|preparing|downloading|postprocessing|finished) echo \"\$s\" ;; esac
")" || { docker logs "$NAME" | tail -60; die "the item was stranded by the restart"; }
ok "the item is $resumed after the restart -- it was resumed, not canceled"

msg="$(req "${BASE}/api/v2/items/${rid}" | jget msg)"
printf '%s' "$msg" | grep -qi 'shutdown\|restart' \
  && ok "its msg says why: ${msg}" \
  || printf '  note the msg is %s\n' "${msg:-<none>}"

# --- 8. no ERROR in the log --------------------------------------------------------------------

log "docker logs"
# The assertion is "the SERVER logged nothing at level ERROR", so the pattern anchors on the
# `tracing` level *field* rather than on the word appearing anywhere in a line.
#
#   text (`.compact()`, the default): `2026-09-04T20:56:07.681341Z  ERROR target: message`
#   json (`LOG_FORMAT=json`):         `{…,"level":"ERROR",…}`
#
# Matching the word anywhere instead flags a *child's* output: `aulos-server` forwards the yt-dlp
# shim's and `bgutil-pot`'s stderr into `tracing` at WARN/INFO, keeping each line's own text, and
# on the shutdown of section 7 the shim writes `ERROR: ytdlp_runner protocol channel failed:
# [Errno 32] Broken pipe` — the parent closing the pipe it was killed through, i.e. the shutdown
# working. That line arrives as `… WARN ytdlp.child: ERROR: …`, so the level anchor skips it while
# still catching every genuine server error, `bgutil-pot`'s terminal `failed` state included (which
# is why this no longer excludes the `bgutil_pot` target the way a word match had to).
errs=/tmp/aulos-e2e-err.$$
if docker logs "$NAME" 2>&1 \
    | grep -E '^[0-9][0-9T:.-]*Z +ERROR |"level" *: *"ERROR"' > "$errs"; then
  cat "$errs"
  rm -f "$errs"
  fail "the log contains ERROR lines"
else
  rm -f "$errs"
  ok "no ERROR lines"
fi

docker rm -f "$NAME" >/dev/null
docker volume rm -f "$VOLUME" >/dev/null

# --- 9. profile B: a seeded legacy STATE_DIR ---------------------------------------------------

log "profile B: importing a legacy STATE_DIR"
SEED="$(mktemp -d)"
mkdir -p "${SEED}/.metube"
# WP-04's checked-in corpus, not a hand-written file: the legacy format is
# `{schema_version, kind, items: [{key, info}]}`, and a plausible-looking approximation would test
# the importer's error path instead of its happy one.
FIXTURE="${ROOT}/crates/aulos-store/tests/fixtures/state/v2"
[ -d "$FIXTURE" ] || die "the legacy fixture is missing: $FIXTURE"
cp "${FIXTURE}"/*.json "${SEED}/.metube/"
ok "seeded $(ls "${SEED}/.metube" | tr '\n' ' ')"

docker run -d --name "$NAME" ${PLATFORM_ARGS[@]+"${PLATFORM_ARGS[@]}"} \
  -p "127.0.0.1:${PORT}:8081" \
  -v "${SEED}:/downloads" \
  -e PUID="$(id -u)" -e PGID="$(id -g)" -e CHOWN_DIRS=false -e UMASK=077 \
  -e AUDIO_DOWNLOAD_DIR=/downloads/audio \
  -e AULOS_E2E=1 -e TELEGRAM_BOT_ENABLED=false \
  "$IMAGE" >/dev/null || die "docker run (profile B) failed"

for _ in $(seq 1 60); do
  health="$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo starting)"
  [ "$health" = "healthy" ] && break
  sleep 2
done
[ "$health" = "healthy" ] || { docker logs "$NAME" | tail -40; die "profile B never became healthy"; }

report="$(req "${BASE}/api/v2/import-report")"
errors="$(printf '%s' "$report" | jget errors)"
if [ "$errors" = "[]" ]; then
  ok "the import report has zero errors"
else
  printf '%s\n' "$report"
  fail "the import report has errors: $errors"
fi

# The v1 `id` of an imported row is the provider's `media_id`, which is what the legacy client
# keys its list by (DESIGN §11.4). These two come straight out of the fixture.
seeded="$(req "${BASE}/history" | python3 -c '
import json,sys
doc = json.load(sys.stdin)
ids = [row.get("id") for key in ("queue","pending","done") for row in doc.get(key, [])]
print(",".join(str(i) for i in ids))
')"
for want in dQw4w9WgXcQ aBcDeF12345; do
  printf '%s' "$seeded" | grep -q "$want" \
    && ok "the legacy row ${want} was imported with its id preserved" \
    || fail "${want} is missing from history: $seeded"
done

[ -f "${SEED}/.metube/.aulos-imported" ] \
  && ok "the marker file was written" \
  || fail "no .aulos-imported marker"

# The other end of the UMASK knob, on a file the server itself creates: this profile runs with
# `UMASK=077`, so the database SQLite opens must be 600 rather than profile A's 644. Together the
# two prove the entrypoint's `umask` is what the server inherits, not a coincidence of the default.
db_mode="$(docker exec "$NAME" stat -c '%a' /downloads/.metube/aulos.db 2>/dev/null || echo '')"
[ "$db_mode" = "600" ] \
  && ok "UMASK=077 reached the database (mode $db_mode)" \
  || fail "UMASK=077 should make aulos.db 600, got ${db_mode:-<unreadable>}"

# A root the entrypoint had to create itself must be handed to PUID:PGID even under
# CHOWN_DIRS=false -- otherwise a split AUDIO_DOWNLOAD_DIR (which the shipped compose uses) boots
# green and then fails every audio download with EACCES while video downloads work.
#
# Two caveats make this assertion narrower than "every root": (1) roots that already existed on the
# volume (/downloads itself, the seeded STATE_DIR) are deliberately NOT chowned under
# CHOWN_DIRS=false, so only the roots the entrypoint logged as created are checked; (2) macOS bind
# mounts under OrbStack/Docker Desktop do not preserve chown at all (stat reports 0:0 regardless),
# so the check is skipped when a probe chown inside the volume does not stick.
probe_dir="/downloads/.aulos-e2e-chown-probe"
docker exec "$NAME" sh -c "mkdir -p '$probe_dir' && chown $(id -u):$(id -g) '$probe_dir'" >/dev/null 2>&1 || true
probe_owner="$(docker exec "$NAME" stat -c '%u:%g' "$probe_dir" 2>/dev/null || echo '')"
docker exec "$NAME" rm -rf "$probe_dir" >/dev/null 2>&1 || true
if [ "$probe_owner" != "$(id -u):$(id -g)" ]; then
  ok "ownership checks skipped: this bind mount does not preserve chown (probe shows ${probe_owner:-<unreadable>})"
else
  created_roots="$(docker logs "$NAME" 2>&1 | sed -n 's/^Created \(.*\); giving it to .*/\1/p')"
  [ -n "$created_roots" ] || fail "the entrypoint created no roots under profile B; expected at least /downloads/audio"
  for root in $created_roots; do
    owner="$(docker exec "$NAME" stat -c '%u:%g' "$root" 2>/dev/null || echo '')"
    [ "$owner" = "$(id -u):$(id -g)" ] \
      && ok "${root} (created by the entrypoint) is owned by PUID:PGID (${owner})" \
      || fail "${root} should be $(id -u):$(id -g), got ${owner:-<unreadable>}"
  done
fi

# A second start must not import again: that is what stops it resurrecting deleted rows.
docker restart -t 25 "$NAME" >/dev/null || die "profile B restart failed"
for _ in $(seq 1 60); do
  health="$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo starting)"
  [ "$health" = "healthy" ] && break
  sleep 2
done
[ "$health" = "healthy" ] || die "profile B unhealthy after the restart"
again="$(req "${BASE}/api/v2/import-report" | jget imported_at)"
first="$(printf '%s' "$report" | jget imported_at)"
[ "$again" = "$first" ] \
  && ok "the second start did not re-import (imported_at is unchanged)" \
  || fail "imported_at moved from $first to $again -- the importer ran twice"

docker rm -f "$NAME" >/dev/null
rm -rf "$SEED"

# --- verdict -----------------------------------------------------------------------------------

if [ "$FAILED" = "0" ]; then
  printf '\nEND-TO-END: PASS\n'
else
  printf '\nEND-TO-END: FAIL\n' >&2
fi
exit "$FAILED"
