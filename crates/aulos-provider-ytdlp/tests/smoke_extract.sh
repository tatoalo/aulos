#!/usr/bin/env bash
# A REAL `mode=extract` through the shipped shim, inside the shipped image (DESIGN §18.5).
#
# This is the check the yt-dlp master bump exists for: legacy auto-merged on a version-string
# diff, so a build that imported but could no longer extract reached the VPS. `doctor` and a
# bare `import yt_dlp` cannot catch that — only asking yt-dlp for real metadata can.
#
# What it asserts, against the container's own network:
#
#   1. the container becomes `healthy`, i.e. `bgutil-pot` is supervised and answering — the shim
#      is exercised with the POT provider live, which is how it runs in production
#   2. `python3 /app/python/ytdlp_runner.py` accepts one DESIGN §9.2 job on stdin
#   3. the transcript is well formed: `hello` first with a yt-dlp version, then `result ok=true`,
#      then `bye` last, with `n` starting at 1 and no gap
#   4. the extracted entry carries the expected video id and a non-empty title
#   5. the shim exits 0
#
#   AULOS_IMAGE=aulos-server:dev crates/aulos-provider-ytdlp/tests/smoke_extract.sh
#
# Knobs:
#   AULOS_IMAGE          image to test  (default aulos-server:dev)
#   AULOS_SMOKE_URL      the video      (default the Blender Foundation's CC-BY trailer)
#   AULOS_SMOKE_ID       its expected `id`
#   AULOS_SMOKE_TIMEOUT  seconds to wait for the container to become healthy (default 90)
set -euo pipefail

IMAGE="${AULOS_IMAGE:-aulos-server:dev}"
# "Big Buck Bunny" trailer, Blender Foundation, CC-BY: small, stable, and legal to fetch in CI.
URL="${AULOS_SMOKE_URL:-https://www.youtube.com/watch?v=aqz-KE-bpKQ}"
WANT_ID="${AULOS_SMOKE_ID:-aqz-KE-bpKQ}"
TIMEOUT="${AULOS_SMOKE_TIMEOUT:-90}"
NAME="aulos-smoke-$$"
SHIM=/app/python/ytdlp_runner.py

say() { printf '\n=== %s\n' "$*"; }
die() {
  printf '\nSHIM EXTRACT SMOKE: FAIL — %s\n' "$*" >&2
  exit 1
}

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -f "$TRANSCRIPT" 2>/dev/null || true
}
TRANSCRIPT="$(mktemp)"
trap cleanup EXIT

say "starting $IMAGE so bgutil-pot is supervised"
# No published port: everything below goes through `docker exec`, and the HEALTHCHECK the image
# ships is what tells us the server and the sidecar are both up.
docker run -d --name "$NAME" "$IMAGE" >/dev/null

state=starting
deadline=$(( $(date +%s) + TIMEOUT ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  state=$(docker inspect -f '{{.State.Health.Status}}' "$NAME" 2>/dev/null || echo starting)
  # `if` rather than `[ … ] && break`: an AND-list as a loop body's last command is the shape
  # that made the DESIGN §18.2 entrypoint snippet abort under `set -e` (see WP-01's note).
  if [ "$state" = healthy ] || [ "$state" = unhealthy ]; then
    break
  fi
  sleep 2
done
if [ "$state" != healthy ]; then
  docker logs "$NAME" 2>&1 | tail -60 >&2 || true
  die "the container never became healthy (last state: $state)"
fi

say "one mode=extract job through $SHIM"
# The DESIGN §9.2 job object, exactly as `Job::extract` serialises it. `extract.flat` is what the
# provider sends, so this is the same code path a real add takes.
job=$(URL="$URL" python3 - <<'PY'
import json, os
print(json.dumps({
    "v": 1,
    "protocol": 1,
    "job_id": "smoke-extract",
    "mode": "extract",
    "url": os.environ["URL"],
    "options": {},
    "coerce": {},
    "policy": {},
    "extract": {
        "flat": True,
        "noplaylist": True,
        "playlist_end": None,
        "strict_retry": True,
        "stream_entries": True,
        "max_entries": 2000,
    },
}))
PY
)

# fd 3 is not open through `docker exec`, so the shim writes its frames to the stdout it saved
# before redirecting fd 1 to /dev/null — which is the documented fallback and is why this can be
# read as plain stdout. stderr is left on the terminal so a failing build says why.
set +e
printf '%s\n' "$job" | docker exec -i "$NAME" python3 "$SHIM" >"$TRANSCRIPT"
shim_status=$?
set -e

echo "--- transcript ---"
cat "$TRANSCRIPT"
echo "--- end ---"

[ "$shim_status" -eq 0 ] || die "the shim exited $shim_status"

WANT_ID="$WANT_ID" python3 - "$TRANSCRIPT" <<'PY' || die "the transcript did not hold up"
import json, os, sys

want_id = os.environ["WANT_ID"]
frames = []
with open(sys.argv[1], encoding="utf-8") as fh:
    for line in fh:
        line = line.strip()
        if not line:
            continue
        try:
            frames.append(json.loads(line))
        except ValueError as exc:
            sys.exit(f"not a JSON line: {line[:200]!r} ({exc})")

if not frames:
    sys.exit("the shim wrote no frames at all")

kinds = [f.get("t") for f in frames]
if kinds[0] != "hello":
    sys.exit(f"first frame is {kinds[0]!r}, expected 'hello'")
if kinds[-1] != "bye":
    sys.exit(f"last frame is {kinds[-1]!r}, expected 'bye'")
for i, frame in enumerate(frames, start=1):
    if frame.get("n") != i:
        sys.exit(f"frame {i} carries n={frame.get('n')!r}; the sequence has a gap")

hello = frames[0]
if not hello.get("yt_dlp"):
    sys.exit("the hello frame reports no yt-dlp version")

errors = [f for f in frames if f.get("t") == "error"]
if errors:
    sys.exit(f"the shim reported an error frame: {json.dumps(errors[0])}")

results = [f for f in frames if f.get("t") == "result"]
if len(results) != 1:
    sys.exit(f"expected exactly one result frame, got {len(results)}")
if results[0].get("ok") is not True:
    sys.exit(f"the result is not ok: {json.dumps(results[0])}")

# The entry the extraction produced: `entry` frames when streaming, `info` for the root.
bodies = [f.get("entry") or {} for f in frames if f.get("t") in ("entry", "info")]
ids = [b.get("id") for b in bodies if isinstance(b, dict)]
if want_id not in ids:
    sys.exit(f"extracted ids {ids!r} do not include the expected {want_id!r}")
titles = [b.get("title") for b in bodies if isinstance(b, dict) and b.get("id") == want_id]
if not any(titles):
    sys.exit(f"the entry for {want_id!r} has no title: {titles!r}")

print(f"yt-dlp {hello['yt_dlp']} extracted {want_id!r} as {titles[0]!r}")
print(f"OK — {len(frames)} frames, one result, no error")
PY

say "SHIM EXTRACT SMOKE: PASS"
