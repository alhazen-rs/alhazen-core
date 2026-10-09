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

# Fragmented MP4 (CMAF) VOD: HEVC + AAC, 2 s segments.
rm -rf fmp4 && mkdir fmp4
ffmpeg -v error -y -f lavfi -t 4 -i "$(v 320x180)" -f lavfi -t 4 -i "$(a)" \
  -c:v libx265 -tag:v hvc1 -pix_fmt yuv420p -x265-params keyint=25:min-keyint=25:scenecut=0:log-level=error \
  -c:a aac -b:a 64k -ac 1 \
  -f hls -hls_time 2 -hls_playlist_type vod -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 \
  -hls_segment_filename 'fmp4/seg%d.m4s' fmp4/index.m3u8

# Packed audio (Apple's format for separate audio): raw ADTS segments, each preceded by an ID3v2.4
# tag whose PRIV frame "com.apple.streaming.transportStreamTimestamp" holds the 90 kHz time of
# its first sample. Starts at 10 s (900000) like a typical TS clock.
rm -rf packed && mkdir packed
ffmpeg -v error -y -f lavfi -t 4 -i "$(a)" -c:a aac -b:a 64k -ac 1 \
  -f segment -segment_time 2 -segment_format adts packed/raw%d.aac
python3 - <<'PY'
import struct, glob
def syncsafe(n): return bytes([(n >> 21) & 0x7F, (n >> 14) & 0x7F, (n >> 7) & 0x7F, n & 0x7F])
def frames(data):
    i = n = 0
    while i + 7 <= len(data):
        i += ((data[i+3] & 3) << 11) | (data[i+4] << 3) | (data[i+5] >> 5); n += 1
    return n
start = 900_000
lines = ["#EXTM3U", "#EXT-X-VERSION:3", "#EXT-X-TARGETDURATION:3", "#EXT-X-MEDIA-SEQUENCE:0", "#EXT-X-PLAYLIST-TYPE:VOD"]
for k in range(len(glob.glob("packed/raw*.aac"))):
    data = open(f"packed/raw{k}.aac", "rb").read()
    body = b"com.apple.streaming.transportStreamTimestamp\0" + struct.pack(">Q", start)
    frame = b"PRIV" + syncsafe(len(body)) + b"\0\0" + body
    open(f"packed/seg{k}.aac", "wb").write(b"ID3\x04\0\0" + syncsafe(len(frame)) + frame + data)
    n = frames(data)
    lines += [f"#EXTINF:{n * 1024 / 48000:.6f},", f"seg{k}.aac"]
    start += n * 1024 * 90_000 // 48_000
open("packed/index.m3u8", "w").write("\n".join(lines + ["#EXT-X-ENDLIST", ""]))
PY
rm packed/raw*.aac
