# alhazen-core

[![crates.io](https://img.shields.io/crates/v/alhazen-core.svg)](https://crates.io/crates/alhazen-core)
[![docs.rs](https://img.shields.io/docsrs/alhazen-core)](https://docs.rs/alhazen-core)
[![CI](https://github.com/alhazen-rs/alhazen-core/actions/workflows/ci.yml/badge.svg)](https://github.com/alhazen-rs/alhazen-core/actions/workflows/ci.yml)

**A native media engine for Rust.** Give it a file or a URL; it hands you video frames on time
and plays the sound. Pure-Rust decoders everywhere, the operating system's GPU decoders where
available, and no GStreamer, bundled ffmpeg or browser engine.

alhazen-core has no UI dependency. Use it to build a video player in any toolkit: GPUI (see
[alhazen-gpui](https://github.com/alhazen-rs/alhazen-gpui)), egui, iced, Slint, a game engine,
or your own renderer.

> Named after **Ibn al-Haytham** (Alhazen, c. 965–1040), whose *Book of Optics* first
> explained how images form, the science behind every camera and screen.

```rust
use alhazen_core::{Player, PlayerConfig, Source};

let player = Player::open(Source::parse("https://example.com/movie.mkv")?, PlayerConfig::default())?;
player.play();
// In your render loop:
if let Some(frame) = player.current_frame() { /* draw it */ }
```

## Features

- **Plays out of the box, on every platform:** AV1, VP9 (multi-threaded), VP8, ProRes, Opus,
  Vorbis, FLAC and PCM in pure Rust, under permissive licences.
- **The OS's own decoders on Windows:** H.264, HEVC, VP9 and AV1 on the GPU, plus AAC, MP3,
  AC-3, E-AC-3 and ALAC, through Media Foundation. Nothing to install; Windows covers the
  codec licences.
- **Everything else through the user's ffmpeg, if they have one:** found at runtime and run as
  a separate program, so nothing is linked and ffmpeg's licence never touches your app.
- **Files and the web:** local files, `file://`, and HTTP(S) streaming with `Range` seeking and
  automatic reconnects.
- **A/V sync done right:** audio drives the clock; video follows, drops late frames and catches
  up after stalls.
- **Smooth on big files:** frames are scaled to the size you display them at before colour
  conversion, and a decoder that can't keep up hands over to a faster one automatically.

## Contents

- [Getting started](#getting-started)
- [Integrating with a UI toolkit](#integrating-with-a-ui-toolkit)
- [Supported formats](#supported-formats)
- [How decoding is chosen](#how-decoding-is-chosen)
- [Cargo features](#cargo-features)
- [Configuration](#configuration)
- [Events and state](#events-and-state)
- [Environment variables](#environment-variables)
- [Platform requirements](#platform-requirements)
- [Performance](#performance)
- [Development](#development)
- [License](#license)

## Getting started

```toml
[dependencies]
alhazen-core = "0.2"

# Decoding unoptimized is many times slower than real time: optimize the decoders even in
# debug builds.
[profile.dev.package.rav1d]
opt-level = 3
[profile.dev.package.vp9-mt]
opt-level = 3
```

```rust
use std::time::Duration;
use alhazen_core::{Player, PlayerConfig, PlayerEvent, Source, VideoFrame};

fn main() -> alhazen_core::Result<()> {
    let player = Player::open(Source::parse("movie.mkv")?, PlayerConfig::default())?;
    let events = player.events(); // crossbeam Receiver<PlayerEvent>
    player.play();

    loop {
        // The frame due now, by the audio clock. Call it every time you draw.
        if let Some(VideoFrame::Cpu { width, height, bgra, pts }) = player.current_frame() {
            // bgra: tightly packed BGRA, width * 4 bytes per row. Upload it and draw.
        }
        for event in events.try_iter() {
            match event {
                PlayerEvent::Warning(w) => eprintln!("warning: {w}"),
                PlayerEvent::Ended => return Ok(()),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}
```

`Player` is `Send + Sync`; keep it in an `Arc` and call it from any thread.

| Control | Query |
|---|---|
| `play()`, `pause()` | `state()` |
| `seek(Duration)` | `position()`, `duration()`, `is_seekable()` |
| `set_volume(0.0..=1.0)`, `set_muted(bool)` | `volume()`, `is_muted()` |
| `set_max_output_size(Option<(w, h)>)` | `has_video()`, `has_audio()`, `video_size()` |
| | `current_frame()`, `events()`, `stats()` |

## Integrating with a UI toolkit

A player in any toolkit comes down to three things:

1. **Draw `current_frame()` every frame while playing.** It returns the frame due at the
   current audio position. Cache the texture and re-upload only when `pts` changes.
2. **Tell the engine how big you draw:** `player.set_max_output_size(Some((w, h)))` with the
   view's size in physical pixels. 4K frames shown in a small view are then scaled down
   before colour conversion, which saves most of the CPU time.
3. **Listen to `events()`** to repaint when paused (`FrameReady`), and to show `Warning`s and
   errors.

[alhazen-gpui](https://github.com/alhazen-rs/alhazen-gpui) is a complete example: about 450
lines for the player entity and the element.

## Supported formats

### Sources

| Source | Example | Notes |
|---|---|---|
| Local file | `movie.mkv`, `C:\Videos\a.mp4` | |
| File URL | `file:///home/me/a.webm` | |
| HTTP(S) | `https://host/a.webm` | Seeks with `Range` requests when the server allows it; reconnects with backoff. |
| HLS / DASH | `https://host/a.m3u8` | Recognised, not playable yet (roadmap). |

### Containers

| Container | Extensions |
|---|---|
| Matroska / WebM | `.mkv`, `.webm`, `.mka` |
| ISO BMFF / QuickTime | `.mp4`, `.m4v`, `.m4a`, `.mov` |

Raw elementary streams (a bare `.mp3`, `.aac`, `.flac`, `.h264`) need a container for now.

### Video

| Codec | Pure Rust (all platforms) | Windows (Media Foundation) | User's ffmpeg |
|---|---|---|---|
| AV1 | ✅ rav1d | ✅ GPU¹ | ✅ |
| VP9 | ✅ vp9-mt, multi-threaded | ✅ GPU¹ | ✅ |
| VP8 | ✅ oximedia-codec | | ✅ |
| Apple ProRes (422, 4444, interlaced) | ✅ oxideav-prores | | ✅ |
| H.264 / AVC | | ✅ GPU or software | ✅ |
| H.265 / HEVC | | ✅ GPU² | ✅ |
| MJPEG, MPEG-4 Part 2, others | | | ✅ |

¹ Used when the GPU decodes it; otherwise the pure-Rust decoder is used (see `prefer_hardware`).
² Needs the HEVC Video Extensions from the Microsoft Store (preinstalled on many PCs).

8- and 10-bit video is supported. Frames are delivered as 8-bit BGRA, converted with the
stream's BT.601/BT.709 matrix and range.

### Audio

| Codec | Pure Rust (all platforms) | Windows (Media Foundation) | User's ffmpeg |
|---|---|---|---|
| Opus (incl. 5.1/7.1) | ✅ | | ✅ |
| Vorbis | ✅ lewton | | ✅ |
| FLAC | ✅ claxon | | ✅ |
| PCM (8–32-bit int, 32/64-bit float) | ✅ | | ✅ |
| AAC (LC, HE) | opt-in `native-aac` (MPL-2.0) | ✅ | ✅ |
| MP3 | | ✅ | ✅ |
| AC-3, E-AC-3 | | ✅ | ✅ |
| ALAC | | ✅ | ✅ |

Audio is resampled to the output device's rate and mixed to its channel layout.

If a file's audio can't be decoded, the video still plays and a `Warning` explains why. If its
video can't be decoded, `Player::open` fails with `Error::UnsupportedCodec`, listing the backends
it tried.

## How decoding is chosen

Each stream is offered to the backends in priority order; the first that claims it decodes it.

| Backend | Platforms | Claims |
|---|---|---|
| `media-foundation` | Windows | H.264/HEVC/audio whenever Windows has a decoder. VP9/AV1 only with a GPU decoder (Windows' software ones are no faster than ours). |
| `native` | all | The pure-Rust decoders. |
| `ffmpeg-cli` | all | Whatever the user's `ffmpeg -decoders` lists. GPU decoding with `-hwaccel auto`. |

- `PlayerConfig::prefer_hardware = false` puts `native` before `media-foundation` for VP9/AV1.
- **Automatic fallback:** if a decoder falls behind (more than 100 ms late for 1.5 s), the
  stream switches once to the next backend that can decode it, at the same position.
  `auto_fallback = false` turns this off.
- `backend_order = Some(vec!["ffmpeg-cli", "native"])` tries backends in your order. For full
  control, build a `backend::Registry` and pass it as `PlayerConfig::registry`.
- `player.stats().video_backend` tells you which one is decoding.

ffmpeg is looked for in `PlayerConfig::ffmpeg.path`, then `$ALHAZEN_FFMPEG`, then `PATH`, and on
macOS also Homebrew's and MacPorts' locations (Finder-launched apps don't get the shell's
`PATH`). On Windows it runs inside a Job Object, so closing the player also kills the real
ffmpeg behind a Chocolatey/Scoop shim.

## Cargo features

| Feature | Default | Adds |
|---|---|---|
| `native` | ✅ | Pure-Rust decoders (AV1, VP9, VP8, ProRes, Opus, Vorbis, FLAC) and the MP4/MOV demuxer. |
| `http` | ✅ | HTTP(S) sources (`ureq`, rustls). |
| `audio-output` | ✅ | Sound through the default output device (`cpal`). Without it, audio is ignored and video runs on the system clock. |
| `ffmpeg-cli` | ✅ | The runtime ffmpeg backend. Costs nothing when ffmpeg isn't installed. |
| `media-foundation` | ✅ | Windows' decoders. Compiles to nothing on other platforms. |
| `native-aac` | | AAC-LC through Symphonia. **MPL-2.0**: closed-source apps are fine, but changes to Symphonia's own files must be shared. |

## Configuration

`PlayerConfig` (all fields public; start from `PlayerConfig::default()`):

| Field | Default | Meaning |
|---|---|---|
| `autoplay` | `false` | Start as soon as the first frame is ready. |
| `audio_output` | `Default` | `Default` (system device), `Disabled` (ignore audio), or `Null(..)` (you pull the samples: tests, offline rendering). |
| `prefer_hardware` | `true` | Prefer GPU decoders (Windows) over the pure-Rust VP9/AV1 decoders. |
| `auto_fallback` | `true` | Switch backend when decoding can't keep up. |
| `backend_order` | `None` | Backend names to try first. |
| `ffmpeg` | default | `enabled`, `path`, `hwaccel` (default `true`). |
| `max_output_size` | `None` | Largest frame size to produce; larger frames are scaled down. |
| `decoder_threads` | cores, max 8 | Threads inside a video decoder. |
| `thread_pool` | shared | Rayon pool for colour conversion and scaling. |
| `frame_queue_len` / `packet_queue_len` | `4` / `64` | Buffering ahead. |
| `registry` | `None` | A custom backend `Registry`. |
| `clock` | `None` | Clock when there is no audio (tests inject a mock). |

## Events and state

`PlayerState`: `Loading` → `Buffering` → `Playing` ⇄ `Paused` → `Ended`, or `Error(e)` at any
point. Seeking passes through `Buffering`.

| `PlayerEvent` | When |
|---|---|
| `StateChanged(state)` | Every state change. |
| `FrameReady` | The first frame after opening or seeking is ready; repaint even when paused. |
| `Warning(String)` | Something non-fatal: an audio track without a decoder, no audio device, a backend fallback. |
| `Error(e)` | Playback stopped with an error. |
| `Ended` | The end was reached. |

## Environment variables

| Variable | Effect |
|---|---|
| `ALHAZEN_FFMPEG=/path/to/ffmpeg` | The ffmpeg program to use (when `PlayerConfig::ffmpeg.path` isn't set). |
| `RUST_LOG=alhazen_core=debug` | Library logging through the `log` crate, with any logger. |

## Platform requirements

- **Rust** 1.92+.
- **x86-64 with the `native` feature:** [`nasm`](https://www.nasm.us/) on `PATH`; rav1d assembles
  its fast AV1 code with it (`apt install nasm`, `brew install nasm`, `winget install NASM.NASM`).
- **Linux with `audio-output`:** ALSA headers (`libasound2-dev` / `alsa-lib`).
- **Windows:** Windows 10 or 11. Media Foundation is missing from "N" editions until the
  Media Feature Pack is installed; the engine then falls back to the other backends.
- **ffmpeg** (optional, at runtime): 4.0+. CI tests 6–9 on Linux, Windows and macOS.

## Performance

- **VP9:** [vp9-mt](https://github.com/alhazen-rs/vp9-mt) decodes tile columns in parallel,
  bit-identical to libvpx: 45 fps vs 17.5 fps single-threaded on a 4K 10-bit stream.
- **GPU on Windows:** H.264/HEVC/VP9/AV1 decode on the GPU's video engine; frames are copied
  back once (NV12/P010) and converted on the CPU.
- **Scale first:** a 4K frame shown at 1280×720 is scaled down in YUV before conversion.
- **Catch-up:** after a stall, frames more than 50 ms late are skipped (one is still shown per
  100 ms), so video rejoins the audio quickly.

## Development

```bash
cargo test                                        # everything (ffmpeg tests skip without ffmpeg)
cargo clippy --all-targets -- -D warnings
python3 scripts/fetch_vp9_vectors.py target/vp9-vectors
VP9_VECTORS_DIR=target/vp9-vectors cargo test --release --test vp9_conformance
```

- Test fixtures and the ffmpeg commands that made them:
  [`tests/fixtures/README.md`](tests/fixtures/README.md).
- On Windows, `cargo run --release --example mf-check -- tests/fixtures` reports, per codec,
  which Media Foundation decoder ran (GPU or software) and whether it decoded correctly.
  `mf-check --native-type <file>` prints the media types Windows' own demuxer reports.
- CI runs on Linux, Windows and macOS, and checks that the default build stays permissively
  licensed.

## License

Apache License, Version 2.0 ([LICENSE](LICENSE)). If you distribute software that contains
alhazen-core, include the [NOTICE](NOTICE) text (Apache-2.0 §4(d)), for example in an "About" or
"Licenses" screen.

The default build depends only on permissively licensed crates (MIT, Apache-2.0, BSD).
`native-aac` adds Symphonia (MPL-2.0). `ffmpeg-cli` links nothing: ffmpeg's licence applies to
the ffmpeg program the user installed, not to your app; don't bundle an ffmpeg build without
checking its licence and patent terms.

Part of [Alhazen](https://github.com/alhazen-rs).
