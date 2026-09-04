#!/bin/sh
# Writes a partial output, then hangs in a grandchild.
out=""
for a in "$@"; do out=$a; done
mkdir -p "$(dirname "$out")"
printf 'partial' > "$out"
sleep 300 &
wait
