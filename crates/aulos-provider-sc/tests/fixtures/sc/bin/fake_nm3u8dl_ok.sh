#!/bin/sh
# A successful N_m3u8DL-RE: two reads of captured Spectre.Console repaints, then the muxed mp4.
here=$(dirname "$0")
save_dir=""
save_name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --save-dir) save_dir=$2; shift 2 ;;
    --save-name) save_name=$2; shift 2 ;;
    *) shift ;;
  esac
done
cat "$here/../nm3u8_repaints.txt"
sleep 0.55
cat "$here/../nm3u8_repaints.txt"
printf '\nDone.\n'
mkdir -p "$save_dir"
printf 'fake-nm3u8-mp4' > "$save_dir/$save_name.mp4"
