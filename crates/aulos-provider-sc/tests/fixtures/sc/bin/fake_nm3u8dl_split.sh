#!/bin/sh
save_dir=""
save_name=""
while [ $# -gt 0 ]; do
  case "$1" in
    --save-dir) save_dir=$2; shift 2 ;;
    --save-name) save_name=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf 'Vid 4/'
sleep 0.1
printf '12 33.33%%'
sleep 0.1
printf ' 369.02KB/'
sleep 0.1
printf '2.16MB 369.02KBps'
sleep 0.1
printf ' 00:00:06\n'
while [ ! -f "$save_dir/allow-finish" ]; do sleep 0.05; done
printf 'fake-nm3u8-mp4' > "$save_dir/$save_name.mp4"
