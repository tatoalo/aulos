#!/bin/sh
# Exits 0 having left only a segment directory: the case the gapless mux exists for.
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
printf 'one' > "$save_dir/$save_name/seg1.ts"
printf 'two' > "$save_dir/$save_name/seg2.ts"
printf 'ten' > "$save_dir/$save_name/seg10.ts"
printf 'Muxing failed, segments kept.\n'
