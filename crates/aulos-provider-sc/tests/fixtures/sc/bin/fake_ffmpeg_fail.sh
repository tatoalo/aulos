#!/bin/sh
# A failing ffmpeg, with the reason on stderr where the error tail reads it.
printf 'Input #0, hls, from ...\n' >&2
printf "https://vixcloud.co/playlist/1: Server returned 403 Forbidden (access denied)\n" >&2
exit 3
