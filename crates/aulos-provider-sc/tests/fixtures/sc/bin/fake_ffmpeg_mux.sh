#!/bin/sh
# A successful remux that records its argv next to the output, so a test can assert exactly one
# ffmpeg ran and that `-f concat` never appeared.
out=""
for a in "$@"; do out=$a; done
log="$(dirname "$out")/ffmpeg-invocations.log"
{
  printf -- '--\n'
  for a in "$@"; do printf '%s\n' "$a"; done
} >> "$log"
mkdir -p "$(dirname "$out")"
printf 'fake-remuxed-mp4' > "$out"
