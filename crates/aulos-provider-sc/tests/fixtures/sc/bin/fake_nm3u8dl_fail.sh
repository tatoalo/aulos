#!/bin/sh
# A failing N_m3u8DL-RE that leaves a partial mp4 and a segment directory behind.
save_dir=""
save_name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --save-dir) save_dir=$2; shift 2 ;;
    --save-name) save_name=$2; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p "$save_dir/$save_name"
printf 'half' > "$save_dir/$save_name.mp4"
printf 'seg' > "$save_dir/$save_name/seg1.ts"
printf 'Loading URL...\n'
printf 'ERROR: master playlist rejected (403)\n'
exit 7
