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

# MPEG-TS VOD: H.264 + AAC, 1 s segments, 6 s (also served as a sliding live window in tests).
rm -rf ts && mkdir ts
ffmpeg -v error -y -f lavfi -t 6 -i "$(v 320x180)" -f lavfi -t 6 -i "$(a)" \
  -c:v libx264 -g 25 -pix_fmt yuv420p -x264-params log-level=error:scenecut=0 -c:a aac -b:a 64k -ac 1 \
  -f hls -hls_time 1 -hls_playlist_type vod -hls_segment_filename 'ts/seg%d.ts' ts/index.m3u8

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

# Master playlist: two H.264 variants (640x360, 320x180) without audio, and the audio as a
# separate rendition (EXT-X-MEDIA TYPE=AUDIO), 1 s segments, 6 s.
rm -rf multi && mkdir multi
ffmpeg -v error -y -f lavfi -t 6 -i "$(v 640x360)" -f lavfi -t 6 -i "$(a)" \
  -filter_complex "[0:v]split=2[hi][lo0];[lo0]scale=320:180[lo]" -map "[hi]" -map "[lo]" -map 1:a \
  -c:v libx264 -g 25 -pix_fmt yuv420p -x264-params log-level=error:scenecut=0 -b:v:0 600k -b:v:1 150k \
  -c:a aac -b:a 64k -ac 1 \
  -var_stream_map "v:0,agroup:aud,name:hi v:1,agroup:aud,name:lo a:0,agroup:aud,default:yes,language:en,name:audio" \
  -master_pl_name master.m3u8 -f hls -hls_time 1 -hls_playlist_type vod \
  -hls_segment_filename 'multi/%v/seg%d.ts' multi/%v/index.m3u8

# AES-128: the TS fixture encrypted with a fixed key and IV.
rm -rf aes && mkdir aes
printf '0123456789abcdef' > aes/key.bin
printf 'key.bin\naes/key.bin\n000102030405060708090a0b0c0d0e0f\n' > aes/keyinfo
ffmpeg -v error -y -f lavfi -t 3 -i "$(v 320x180)" -f lavfi -t 3 -i "$(a)" \
  -c:v libx264 -g 25 -pix_fmt yuv420p -x264-params log-level=error:scenecut=0 -c:a aac -b:a 64k -ac 1 \
  -hls_key_info_file aes/keyinfo -f hls -hls_time 1 -hls_playlist_type vod \
  -hls_segment_filename 'aes/seg%d.ts' aes/index.m3u8
rm aes/keyinfo

# A discontinuity: 3 s of the TS fixture, then a separate encode at another size (its clock
# starts over).
rm -rf disc && mkdir disc
ffmpeg -v error -y -f lavfi -t 3 -i "$(v 160x90)" -f lavfi -t 3 -i "sine=frequency=880:sample_rate=48000" \
  -c:v libx264 -g 25 -pix_fmt yuv420p -x264-params log-level=error:scenecut=0 -c:a aac -b:a 64k -ac 1 \
  -f hls -hls_time 1 -hls_playlist_type vod -hls_segment_filename 'disc/b%d.ts' disc/b.m3u8
{
  printf '#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n'
  for i in 0 1 2; do printf '#EXTINF:1.000000,\n../ts/seg%d.ts\n' $i; done
  printf '#EXT-X-DISCONTINUITY\n'
  grep -A1 '^#EXTINF' disc/b.m3u8 | grep -v '^--$'
  printf '#EXT-X-ENDLIST\n'
} > disc/index.m3u8
rm disc/b.m3u8

# MPEG-TS whose 33-bit clock wraps mid-stream (2^33 / 90 kHz = 95443.7 s): starts at 95441 s.
rm -rf wrap && mkdir wrap
ffmpeg -v error -y -f lavfi -t 6 -i "$(v 320x180)" -f lavfi -t 6 -i "$(a)" \
  -c:v libx264 -g 25 -pix_fmt yuv420p -x264-params log-level=error:scenecut=0 -c:a aac -b:a 64k -ac 1 \
  -output_ts_offset 95441 -f hls -hls_time 1 -hls_playlist_type vod -hls_segment_filename 'wrap/seg%d.ts' wrap/index.m3u8

# A .ts file with 10 s GOPs (x264's default keyint at 25 fps), 12 s: seeking must not scan the file.
ffmpeg -v error -y -f lavfi -t 12 -i "testsrc=size=160x90:rate=25" \
  -c:v libx264 -g 250 -pix_fmt yuv420p -x264-params log-level=error:scenecut=0 -f mpegts ../long_gop.ts
