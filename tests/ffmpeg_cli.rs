//! The `ffmpeg-cli` backend against the real `ffmpeg` on PATH. Each test prints a note and passes
//! when ffmpeg is not installed (CI installs it on every OS).
#![cfg(all(feature = "ffmpeg-cli", feature = "native"))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use video_core::audio::{AudioOutputConfig, NullOutput};
use video_core::backend::{Backend, Registry};
use video_core::decode::{DecodedFrame, PixelLayout};
use video_core::demux::{Demuxer, MatroskaDemuxer, Mp4Demuxer, StreamKind};
use video_core::ffmpeg::FfmpegCliBackend;
use video_core::source::FileSource;
use video_core::{Error, FfmpegConfig, Player, PlayerConfig, PlayerState, Source};

const RATE: u32 = 48_000;

fn fixture_path(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn backend() -> Option<FfmpegCliBackend> {
    let b = FfmpegCliBackend::new(FfmpegConfig { hwaccel: false, ..Default::default() });
    if b.ffmpeg().is_none() {
        eprintln!("skipped: ffmpeg not found on PATH");
        return None;
    }
    Some(b)
}

fn until<T>(what: &str, secs: u64, mut step: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = step() {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn play_ms(null: &NullOutput, ms: u64) {
    for _ in 0..ms.div_ceil(10) {
        null.pull((RATE / 100) as usize);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn decodes_h264_from_mp4_with_packet_timestamps() {
    let Some(b) = backend() else { return };
    let mut d = Mp4Demuxer::open(Box::new(FileSource::open(fixture_path("h264_aac.mp4")).unwrap())).unwrap();
    let stream = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    assert!(b.supports_video(&stream));
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    let mut pts = vec![];
    let mut frames = vec![];
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != stream.id {
            continue;
        }
        pts.push(p.pts);
        dec.send_packet(&p).unwrap();
        while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            frames.push(f);
        }
    }
    dec.send_eof();
    while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
        frames.push(f);
    }
    pts.sort();
    assert_eq!(frames.len(), 30);
    assert_eq!(frames.iter().map(|f| f.pts).collect::<Vec<_>>(), pts, "presentation order, packet pts");
    let f = &frames[0];
    assert_eq!((f.width, f.height, f.layout, f.planes[0].len()), (320, 240, PixelLayout::I420, 320 * 240));
}

#[test]
fn decodes_hevc_from_matroska() {
    let Some(b) = backend() else { return };
    let mut d = MatroskaDemuxer::open(Box::new(FileSource::open(fixture_path("hevc.mkv")).unwrap())).unwrap();
    let stream = d.streams()[0].clone();
    if !b.supports_video(&stream) {
        eprintln!("skipped: this ffmpeg has no HEVC decoder");
        return;
    }
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    let mut n = 0;
    while let Some(p) = d.next_packet().unwrap() {
        dec.send_packet(&p).unwrap();
        while dec.receive_frame().unwrap().is_some() {
            n += 1;
        }
    }
    dec.send_eof();
    while dec.receive_frame().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 30);
}

#[test]
fn decodes_aac_audio() {
    let Some(b) = backend() else { return };
    let mut d = Mp4Demuxer::open(Box::new(FileSource::open(fixture_path("h264_aac.mp4")).unwrap())).unwrap();
    let stream = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
    assert!(b.supports_audio(&stream));
    let mut dec = b.open_audio_decoder(&stream).unwrap();
    let mut buffers = vec![];
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream == stream.id {
            dec.send_packet(&p).unwrap();
            while let Some(buf) = dec.receive_samples().unwrap() {
                buffers.push(buf);
            }
        }
    }
    dec.send_eof();
    while let Some(buf) = dec.receive_samples().unwrap() {
        buffers.push(buf);
    }
    let frames: usize = buffers.iter().map(|b| b.samples.len() / b.channels as usize).sum();
    assert_eq!((buffers[0].rate, buffers[0].channels), (44_100, 1));
    // 1 s of a 440 Hz tone (AAC adds up to one 1024-sample frame of priming/padding).
    assert!((44_100..=44_100 + 2 * 1024).contains(&frames), "{frames} sample frames");
    assert!(buffers.windows(2).all(|w| w[0].pts < w[1].pts));
    let peak = buffers.iter().flat_map(|b| &b.samples).fold(0f32, |m, s| m.max(s.abs()));
    // ffmpeg's `sine` source has amplitude 1/8.
    assert!(peak > 0.08, "silent output ({peak})");
}

#[test]
fn h264_aac_plays_to_the_end_in_sync() {
    if backend().is_none() {
        return;
    }
    let null = NullOutput::new(RATE, 2);
    let config = PlayerConfig { decoder_threads: 2, audio_output: AudioOutputConfig::Null(null.clone()), ..Default::default() };
    let player = Player::open(Source::parse(&fixture_path("h264_aac.mp4")).unwrap(), config).unwrap();
    assert!(player.has_video() && player.has_audio(), "AAC goes to ffmpeg when native-aac is off");
    player.play();
    let mut checked = 0;
    until("end of playback", 20, || {
        play_ms(&null, 10);
        if let Some(f) = player.current_frame() {
            let pos = player.position();
            if pos > Duration::from_millis(100) && pos < Duration::from_millis(900) {
                let diff = pos.abs_diff(f.pts());
                assert!(diff <= Duration::from_millis(45), "A/V offset {diff:?} at {pos:?}");
                checked += 1;
            }
        }
        (player.state() == PlayerState::Ended).then_some(())
    });
    assert!(checked > 30, "only {checked} sync checks");
}

#[test]
fn seeking_restarts_ffmpeg_at_the_target() {
    if backend().is_none() {
        return;
    }
    let config = PlayerConfig { decoder_threads: 2, audio_output: AudioOutputConfig::Disabled, ..Default::default() };
    let player = Player::open(Source::parse(&fixture_path("hevc.mkv")).unwrap(), config).unwrap();
    until("first frame", 10, || player.current_frame());
    for target in [700u64, 200, 500, 950] {
        player.seek(Duration::from_millis(target));
        let target = Duration::from_millis(target);
        // Accurate seek: the frame shown is the one whose display interval (33 ms) holds the target.
        until("frame at the seek target", 10, || {
            player.current_frame().filter(|f| f.pts() <= target && f.pts() + Duration::from_millis(34) > target)
        });
    }
}

/// The first frame must come out while input is still streaming in, not only at end of input
/// (ffmpeg would otherwise buffer seconds of packets to probe the stream).
#[test]
fn first_frame_arrives_before_end_of_input() {
    let Some(b) = backend() else { return };
    let mut d = Mp4Demuxer::open(Box::new(FileSource::open(fixture_path("h264_aac.mp4")).unwrap())).unwrap();
    let stream = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    let mut sent = 0;
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != stream.id {
            continue;
        }
        dec.send_packet(&p).unwrap();
        sent += 1;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(100) {
            if dec.receive_frame().unwrap().is_some() {
                assert!(sent < 30, "first frame only after all input");
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    panic!("no frame before end of input");
}

#[test]
fn missing_ffmpeg_means_unsupported_codec_not_a_hang() {
    let config = PlayerConfig {
        audio_output: AudioOutputConfig::Disabled,
        ffmpeg: FfmpegConfig { path: Some(PathBuf::from("/nonexistent/ffmpeg-for-video-core-tests")), ..Default::default() },
        ..Default::default()
    };
    match Player::open(Source::parse(&fixture_path("hevc.mkv")).unwrap(), config) {
        Err(Error::UnsupportedCodec { tried_backends, .. }) => assert!(tried_backends.is_empty()),
        other => panic!("expected UnsupportedCodec, got {:?}", other.err()),
    }
}

/// An "ffmpeg" that answers `-version`/`-decoders` like the real one but is killed shortly after
/// it starts decoding.
#[cfg(unix)]
#[test]
fn ffmpeg_killed_mid_stream_is_an_error_not_a_hang() {
    use std::os::unix::fs::PermissionsExt;
    if backend().is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("video-core-dying-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ffmpeg");
    std::fs::write(
        &script,
        "#!/bin/sh\ncase \"$2\" in -version|-decoders) exec ffmpeg \"$@\";; esac\nffmpeg \"$@\" &\nsleep 0.3\nkill -9 $!\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let b = FfmpegCliBackend::new(FfmpegConfig { path: Some(script), hwaccel: false, ..Default::default() });
    let mut d = MatroskaDemuxer::open(Box::new(FileSource::open(fixture_path("hevc.mkv")).unwrap())).unwrap();
    let stream = d.streams()[0].clone();
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    let start = Instant::now();
    let mut result = Ok(());
    // Feed slowly so the kill lands mid-stream.
    while let Some(p) = d.next_packet().unwrap() {
        std::thread::sleep(Duration::from_millis(30));
        result = dec.send_packet(&p).and_then(|_| dec.receive_frame().map(|_| ()));
        if result.is_err() {
            break;
        }
    }
    if result.is_ok() {
        dec.send_eof();
        result = std::iter::from_fn(|| match dec.receive_frame() {
            Ok(Some(_)) => Some(Ok(())),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        })
        .find(|r| r.is_err())
        .unwrap_or(Ok(()));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(result.is_err(), "a killed ffmpeg must surface as an error");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn native_codecs_are_never_sent_to_ffmpeg() {
    let r = Registry::with_defaults();
    let mut d = MatroskaDemuxer::open(Box::new(FileSource::open(fixture_path("vp9_profile0.webm")).unwrap())).unwrap();
    let stream = d.streams()[0].clone();
    let _ = d.next_packet();
    // The native backend claims VP9 first; ffmpeg-cli is never probed for it.
    let names = r.ordered(None).iter().filter(|b| b.supports_video(&stream)).map(|b| b.name()).next();
    assert_eq!(names, Some("native"));
    let _ = Arc::new(());
}
