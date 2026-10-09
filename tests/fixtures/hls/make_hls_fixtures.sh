#!/usr/bin/env bash
# Regenerates the HLS and MPEG-TS fixtures (run from this directory). Needs ffmpeg with libx264,
# libx265 and the native AAC encoder. Every lavfi input has a duration.
set -euo pipefail
cd "$(dirname "$0")"
v() { echo "testsrc=size=$1:rate=25"; }
a() { echo "sine=frequency=440:sample_rate=48000"; }

# A plain .ts file (also the format of most HLS segments): H.264 + AAC, 1 s GOPs, 3 s.
ffmpeg -v error -y -f lavfi -t 3 -i "$(v 320x180)" -f lavfi -t 3 -i "$(a)" \
  -c:v libx264 -g 25 -pix_fmt yuv420p -x264-params log-level=error -c:a aac -b:a 64k -ac 1 \
  -f mpegts ../h264_aac.ts
