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
