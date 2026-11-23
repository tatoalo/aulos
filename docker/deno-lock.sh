#!/bin/sh
# One `deno run` at a time: a YouTube challenge solve peaks near 300 MB.
lock=/usr/local/lib/deno/lock
if [ "$1" = run ] && [ -r "$lock" ]; then
    exec 9<"$lock"
    flock -w 30 9 || true
fi
exec /usr/local/lib/deno/bin/deno "$@"
