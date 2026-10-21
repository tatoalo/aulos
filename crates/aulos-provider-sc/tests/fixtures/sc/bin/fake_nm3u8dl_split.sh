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
printf 'Vid 1280x720 | 1800 Kbps 253/'
sleep 0.1
printf '499 50.70%%'
sleep 0.1
printf ' 360.74MB/'
sleep 0.1
printf '803.62MB 98.18MBps'
sleep 0.1
printf ' 00:00:04\nSub ita | Italian 0/100 0.00%% - - --:--:--\n'
while [ ! -f "$save_dir/allow-finish" ]; do sleep 0.05; done
printf 'Vid 1280x720 | 1800 Kbps 499/499 100.00%% 803.62MB - 00:00:00\nSub ita | Italian 1/1 100.00%% 1.00KB - 00:00:00\n'
printf 'fake-nm3u8-mp4' > "$save_dir/$save_name.mp4"
