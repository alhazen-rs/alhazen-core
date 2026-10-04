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
head -c 4096 /dev/urandom > not_video.bin
```

## Expected properties (the tests rely on these)

- 320×240, 60 frames at 30 fps, duration 2.000 s
- Keyframes at 0 ms and 1000 ms
- WebM files contain Cues
- `av1_with_audio.webm` has an Opus audio track
- `av1_small_clusters.webm` has clusters every ~267 ms that do not line up with keyframes (the 1000 ms keyframe sits inside the cluster starting at 800 ms)
- `truncated.webm` is the first 20000 bytes of `av1.webm`
