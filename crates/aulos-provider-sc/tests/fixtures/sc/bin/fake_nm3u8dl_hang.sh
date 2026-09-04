#!/bin/sh
# Creates the partial output and the segment directory, then hangs in a grandchild so the
# process-group kill is what ends it.
save_dir=""
save_name=""
tmp_dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --save-dir) save_dir=$2; shift 2 ;;
    --save-name) save_name=$2; shift 2 ;;
    --tmp-dir) tmp_dir=$2; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p "$save_dir/$save_name" "$tmp_dir/$save_name" "$tmp_dir/$save_name.tmp"
printf 'half' > "$save_dir/$save_name.mp4"
printf 'seg' > "$save_dir/$save_name/seg1.ts"
sleep 300 &
wait
