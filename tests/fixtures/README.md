# Test fixtures

## How these fixtures were generated

Requires ffmpeg with `libsvtav1` and `libopus`.

```bash
export SVT_LOG=1
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 2 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 av1.webm
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 2 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 -movflags +faststart av1.mp4
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 2 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 -c:a libopus -b:a 32k av1_with_audio.webm
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 2 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 -cluster_time_limit 250 av1_small_clusters.webm
head -c 20000 av1.webm > truncated.webm
# Phase 2 (audio):
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=frequency=440:sample_rate=44100 -t 2 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 -c:a libvorbis -q:a 2 av1_vorbis.webm
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=frequency=440:sample_rate=44100 -t 2 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 -c:a aac -b:a 64k -movflags +faststart av1_aac.mp4
ffmpeg -v error -y -f lavfi -i sine=frequency=440:sample_rate=48000 -t 2 -c:a libopus -b:a 32k opus_only.webm
ffmpeg -v error -y -f lavfi -i sine=frequency=440:sample_rate=44100 -t 2 -c:a aac -b:a 64k -movflags +faststart aac_only.m4a
ffmpeg -v error -y -f lavfi -i sine=frequency=440:sample_rate=48000 -t 2 -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" -c:a libopus -b:a 96k opus_51.webm
ffmpeg -v error -y -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 1 -af "pan=5.1|FL=0*c0|FR=0*c0|FC=c0|LFE=0*c0|BL=0*c0|BR=0*c0" -c:a libvorbis -q:a 3 vorbis_51_center.webm
head -c 4096 /dev/urandom > not_video.bin
```

## Expected properties (the tests rely on these)

- 320×240, 60 frames at 30 fps, duration 2.000 s
- Keyframes at 0 ms and 1000 ms
- WebM files contain Cues
- `av1_with_audio.webm` has an Opus audio track
- `av1_small_clusters.webm` has clusters every ~267 ms that do not line up with keyframes (the 1000 ms keyframe sits inside the cluster starting at 800 ms)
- `truncated.webm` is the first 20000 bytes of `av1.webm`
- Audio fixtures: a 440 Hz sine, 2 s (`vorbis_51_center.webm`: 1 s, tone on the centre channel only)
- `av1_with_audio.webm`: Opus mono 48 kHz; `av1_vorbis.webm`: Vorbis mono 44.1 kHz; `av1_aac.mp4` / `aac_only.m4a`: AAC-LC mono 44.1 kHz
- `opus_51.webm`: 6-channel (multistream) Opus; `opus_51_center.webm`: 1 s, tone on the centre only
- ffmpeg writes `FlagDefault = 0` on audio tracks

## Timing fixtures

```bash
ffmpeg -v error -y -f lavfi -t 2 -i 'aevalsrc=0.2*sin(2*PI*(100+400*t)*t):s=48000:d=2' -t 2 -c:a libopus -b:a 96k chirp_opus.webm
ffmpeg -v error -y -f lavfi -t 2 -i 'aevalsrc=0.2*sin(2*PI*(100+400*t)*t):s=44100:d=2' -t 2 -c:a libvorbis -q:a 6 chirp_vorbis.webm
ffmpeg -v error -y -f lavfi -t 2 -i testsrc2=size=320x240:rate=30 -f lavfi -t 1 -i sine=frequency=440:sample_rate=48000 -pix_fmt yuv420p -c:v libsvtav1 -preset 10 -g 30 -crf 45 -c:a libopus -b:a 32k av1_short_audio.webm
```

- `chirp_*.webm`: reference timestamps come from ffmpeg's decoder
  (`ffprobe -select_streams a:0 -show_entries frame=pts_time,nb_samples`): Opus 0.000/648, 0.014/960, 0.034/960, 0.054/960;
  Vorbis 0.000/576, 0.013/1024, 0.036/1024, 0.060/1024 (presentation time = Matroska block time − CodecDelay)
- `av1_short_audio.webm`: 2 s of video but only 1 s of audio
- Note for zsh: write filter strings literally (or `${VAR}`); `$VAR:s…` is a zsh modifier

## Lacing fixtures

```bash
ffmpeg -v error -y -f lavfi -t 2 -i 'sine=frequency=440:sample_rate=44100' -t 2 -c:a libvorbis -q:a 2 vorbis_only.webm
python3 make_laced.py vorbis_only.webm laced_vorbis.webm
```

- `laced_vorbis.webm`: every 3 Vorbis frames Xiph-laced into one SimpleBlock (29 laced blocks), no
  DefaultDuration (frames in a block share its timestamp), unknown-size Segment, no Cues. mkvmerge
  laces Vorbis this way; `make_laced.py` reproduces it without mkvtoolnix. ffmpeg demuxes it back
  into the same 88 packets as `vorbis_only.webm`.


## Phase 3 fixtures (VP9, VP8, ProRes; ffmpeg-only codecs)

Requires ffmpeg with `libvpx`, `libx264` and `libx265`. Run with bash (zsh does not word-split `$V`).

```bash
V="-f lavfi -i testsrc2=size=320x240:rate=30"
ffmpeg -v error -y $V -t 2 -pix_fmt yuv420p -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -g 30 -crf 50 -b:v 0 vp9_profile0.webm
ffmpeg -v error -y $V -t 2 -pix_fmt yuv420p10le -profile:v 2 -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -g 30 -crf 50 -b:v 0 vp9_10bit.webm
ffmpeg -v error -y -f lavfi -i testsrc2=size=1024x576:rate=30 -t 0.34 -pix_fmt yuv420p -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -tile-columns 2 -crf 55 -b:v 0 vp9_tiles4.webm
ffmpeg -v error -y $V -t 2 -pix_fmt yuv420p -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -g 30 -crf 50 -b:v 0 -movflags +faststart vp9.mp4
ffmpeg -v error -y $V -t 2 -pix_fmt yuv420p -c:v libvpx -deadline realtime -cpu-used 8 -g 30 -crf 50 -b:v 200k vp8.webm
ffmpeg -v error -y -f lavfi -i testsrc2=size=192x128:rate=30 -t 0.2 -c:v prores_ks -profile:v 3 -pix_fmt yuv422p10le prores_hq.mov
ffmpeg -v error -y -f lavfi -i testsrc2=size=192x128:rate=30 -t 0.2 -c:v prores_ks -profile:v 4 -pix_fmt yuv444p10le prores_4444.mov
ffmpeg -v error -y -f lavfi -i testsrc2=size=192x128:rate=30 -t 0.1 -vf setfield=tff -c:v prores_ks -profile:v 4 -pix_fmt yuv444p10le -flags +ildct+ilme prores_4444_interlaced.mov
ffmpeg -v error -y -f lavfi -i testsrc2=size=192x120:rate=30 -t 0.2 -c:v prores_ks -profile:v 4 -pix_fmt yuv444p10le prores_4444_h120.mov
ffmpeg -v error -y -f lavfi -i testsrc2=size=192x120:rate=30 -t 0.1 -vf setfield=tff -c:v prores_ks -profile:v 4 -pix_fmt yuv444p10le -flags +ildct+ilme prores_4444_h120_interlaced.mov
ffmpeg -v error -y $V -t 2 -pix_fmt yuv420p -c:v libx265 -preset fast -crf 40 -x265-params log-level=none:keyint=30:min-keyint=30:open-gop=1:bframes=4 hevc_open_gop.mkv
ffmpeg -v error -y $V -f lavfi -i sine=frequency=440:sample_rate=44100 -t 1 -pix_fmt yuv420p -c:v libx264 -preset ultrafast -crf 40 -g 30 -c:a aac -b:a 48k -movflags +faststart h264_aac.mp4
ffmpeg -v error -y $V -t 1 -pix_fmt yuv420p -c:v libx265 -preset ultrafast -crf 40 -g 30 -x265-params log-level=none hevc.mkv
ffmpeg -v error -y -i vp9_tiles4.webm -c copy -f ivf ../../../vp9-mt/tests/data/tiles4.ivf
```

- VP9/VP8 fixtures: 320×240, 60 frames at 30 fps, keyframes at 0 and 1000 ms (`vp9_10bit.webm` is Profile 2)
- `vp9_tiles4.webm`: 1024×576, 11 frames, 4 tile columns on the key frame
- ProRes: 192×128, 6 frames (`prores_hq.mov` `apch` 4:2:2; `prores_4444*.mov` `ap4h` 4:4:4; the
  interlaced one is 3 frames, top field first)
- `prores_4444_h120*.mov`: 192×120, so the last macroblock row is partial (as at 1080p / 1080i)
- `h264_aac.mp4` (H.264 + AAC-LC 44.1 kHz) and `hevc.mkv`: 1 s, decodable only through ffmpeg
- `hevc_open_gop.mkv`: 2 s, open GOP (CRA keyframes at 0 and 1 s, 4 B-frames): decoding from the
  1 s keyframe makes ffmpeg skip the leading frames

## PCM fixtures

```bash
A="-f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.25 -ac 2"
for spec in "pcm_s24le mov" "pcm_s24be mov" "pcm_s16le mov" "pcm_s16be mov" "pcm_f32le mov" "pcm_u8 mov" \
            "pcm_s16le mp4" "pcm_s16le mkv" "pcm_s24be mkv" "pcm_f32le mkv" "pcm_u8 mkv"; do
  set -- $spec; ffmpeg -v error -y $A -c:a $1 $1.$2
done
ffmpeg -v error -y -f lavfi -i testsrc2=size=128x96:rate=30 -f lavfi -i sine=frequency=440:sample_rate=48000 -t 0.4 -ac 2 -c:v prores_ks -profile:v 0 -pix_fmt yuv422p10le -c:a pcm_s24le prores_pcm.mov
```

- 0.25 s, 48 kHz stereo, 440 Hz at 1/8 amplitude spread at −3 dB (peak 0.0884)
- MOV sample entries: `in24` (with `wave/enda` = little endian for `pcm_s24le`, without for big
  endian), `sowt`, `twos`, `fl32` (+`enda`), `raw `; MP4 `ipcm` (+`pcmC`); Matroska `A_PCM/INT/LIT`,
  `A_PCM/INT/BIG`, `A_PCM/FLOAT/IEEE`, 8-bit unsigned `A_PCM/INT/LIT`
- `prores_pcm.mov`: ProRes Proxy video with 24-bit PCM audio, like camera/editor exports

## Colour-tagged fixtures

```bash
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 0.5 -pix_fmt yuv420p -c:v libx264 -preset ultrafast -crf 40 -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv h264_bt709.mp4
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 0.2 -pix_fmt yuvj420p -c:v mjpeg -q:v 8 mjpeg_full_range.mkv
```

- `h264_bt709.mp4`: 320×240 tagged BT.709 (a height-based guess would say BT.601)
- `mjpeg_full_range.mkv`: full-range (JPEG) MJPEG; Matroska `Colour/Range` = 2

## Resolution change (NVDEC)

```bash
ffmpeg -v error -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 1 -pix_fmt yuv420p -c:v libvpx-vp9 -g 30 -deadline realtime -b:v 200k a.ivf
ffmpeg -v error -y -f lavfi -i testsrc2=size=640x360:rate=30 -t 1 -pix_fmt yuv420p -c:v libvpx-vp9 -g 30 -deadline realtime -b:v 300k b.ivf
printf "file 'a.ivf'\nfile 'b.ivf'\n" > list.txt
ffmpeg -v error -y -f concat -safe 0 -i list.txt -c copy vp9_size_change.webm
```

- `vp9_size_change.webm`: one VP9 track, 30 frames at 320×240 then 30 at 640×360 (a keyframe with
  the new size at 1 s).

## Stream variants GPUs often can't decode (NVDEC fallback)

```bash
S="-f lavfi -i testsrc2=size=320x240:rate=30 -t 1"
ffmpeg -v error -y $S -pix_fmt yuv420p10le -c:v libx264 -profile:v high10 -g 30 -crf 30 h264_10bit.mkv
ffmpeg -v error -y $S -pix_fmt yuv444p -c:v libx265 -x265-params log-level=error -g 30 -crf 30 hevc_444.mkv
ffmpeg -v error -y $S -pix_fmt yuv444p -c:v libvpx-vp9 -deadline realtime -g 30 -b:v 200k vp9_444.webm
```

- `h264_10bit.mkv`: H.264 High 10 ("Hi10P"); `hevc_444.mkv`: HEVC Range Extensions 4:4:4;
  `vp9_444.webm`: VP9 profile 1 (4:4:4). 30 frames each, 320×240.
