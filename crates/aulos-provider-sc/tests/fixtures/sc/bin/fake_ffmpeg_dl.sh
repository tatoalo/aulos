#!/bin/sh
# A successful ffmpeg download: writes the output, then replays a captured -progress stream with a
# pause before each group so the 0.5 s throttle lets it through.
here=$(dirname "$0")
out=""
for a in "$@"; do out=$a; done
mkdir -p "$(dirname "$out")"
printf 'fake-ffmpeg-mp4' > "$out"
while IFS= read -r line; do
  case "$line" in
    progress=*) sleep 0.55 ;;
  esac
  printf '%s\n' "$line"
done < "$here/../ffmpeg_progress.txt"
