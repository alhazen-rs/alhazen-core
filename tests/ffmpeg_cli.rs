//! The `ffmpeg-cli` backend against the real `ffmpeg` on PATH. Each test prints a note and passes
//! when ffmpeg is not installed (CI installs it on every OS).
#![cfg(all(feature = "ffmpeg-cli", feature = "native"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use video_core::audio::{AudioOutputConfig, NullOutput};
use video_core::backend::Backend;
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

/// Starting at an open-GOP (CRA) keyframe, as after a seek, ffmpeg skips the leading frames that
/// reference the previous GOP: fewer frames come out than packets went in. Timestamps must still
/// belong to the frames that are shown.
#[test]
fn frames_keep_their_timestamps_when_ffmpeg_skips_some() {
    let Some(b) = backend() else { return };
    let mut d = MatroskaDemuxer::open(Box::new(FileSource::open(fixture_path("hevc_open_gop.mkv")).unwrap())).unwrap();
    let stream = d.streams()[0].clone();
    if !b.supports_video(&stream) {
        eprintln!("skipped: this ffmpeg has no HEVC decoder");
        return;
    }
    let packets: Vec<_> = std::iter::from_fn(|| d.next_packet().unwrap()).collect();
    let key = packets.iter().position(|p| p.keyframe && p.pts == Duration::from_secs(1)).expect("keyframe at 1 s");
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    let mut shown = vec![];
    for p in &packets[key..] {
        dec.send_packet(p).unwrap();
        while let Some(f) = dec.receive_frame().unwrap() {
            shown.push(f.pts());
        }
    }
    dec.send_eof();
    while let Some(f) = dec.receive_frame().unwrap() {
        shown.push(f.pts());
    }
    let sent: std::collections::BTreeSet<Duration> = packets[key..].iter().map(|p| p.pts).collect();
    assert!(!shown.is_empty());
    assert_eq!(shown[0], Duration::from_secs(1), "the first frame shown is the keyframe: {shown:?}");
    assert!(shown.windows(2).all(|w| w[0] < w[1]), "display order: {shown:?}");
    assert!(shown.iter().all(|p| sent.contains(p)), "every timestamp is a real packet's: {shown:?}");
    assert_eq!(shown.last(), sent.last(), "the last frame keeps the last timestamp");
}

/// The first frame ffmpeg decodes from `name`'s video track.
fn ffmpeg_first_frame(b: &FfmpegCliBackend, name: &str) -> Option<video_core::decode::YuvFrame> {
    let source = Source::parse(&fixture_path(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = video_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let mut d = video_core::backend::Registry::with_defaults().open_demuxer(&source, format, src, None).unwrap();
    let stream = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    if !b.supports_video(&stream) {
        return None;
    }
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream == stream.id {
            dec.send_packet(&p).unwrap();
        }
    }
    dec.send_eof();
    dec.receive_frame().unwrap().map(|DecodedFrame::Yuv(f)| f)
}

/// Colour matrix and range come from the stream (via ffmpeg's output), not a guess by height.
#[test]
fn ffmpeg_frames_carry_the_streams_colour_matrix_and_range() {
    let Some(b) = backend() else { return };
    let f = ffmpeg_first_frame(&b, "h264_bt709.mp4").unwrap();
    // 320x240 would be guessed BT.601; the stream says BT.709.
    assert_eq!((f.matrix, f.full_range), (video_core::decode::ColorMatrix::Bt709, false));
    let Some(f) = ffmpeg_first_frame(&b, "mjpeg_full_range.mkv") else {
        eprintln!("skipped MJPEG: no mjpeg decoder");
        return;
    };
    assert!(f.full_range, "JPEG video is full range");
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

/// ffmpeg installed while the app runs is found on the next lookup (a failed lookup is not
/// remembered).
#[cfg(unix)]
#[test]
fn ffmpeg_installed_later_is_found() {
    use std::os::unix::fs::PermissionsExt;
    if backend().is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("video-core-late-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ffmpeg");
    let b = FfmpegCliBackend::new(FfmpegConfig { path: Some(script.clone()), hwaccel: false, ..Default::default() });
    let h264 = video_core::demux::StreamInfo::new(1, StreamKind::Video, video_core::demux::Codec::H264);
    assert!(!b.supports_video(&h264), "not installed yet");
    std::fs::write(&script, "#!/bin/sh\nexec ffmpeg \"$@\"\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let found = b.supports_video(&h264);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(found, "installed now");
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

/// An "ffmpeg" that is alive but never reads its input (e.g. a hung GPU driver): sending must
/// fail after the stall limit instead of blocking the decode thread forever.
#[cfg(unix)]
#[test]
fn ffmpeg_that_stops_reading_is_an_error_not_a_hang() {
    use std::os::unix::fs::PermissionsExt;
    use video_core::demux::Packet;
    if backend().is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("video-core-stuck-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ffmpeg");
    std::fs::write(&script, "#!/bin/sh\ncase \"$2\" in -version|-decoders) exec ffmpeg \"$@\";; esac\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let b = FfmpegCliBackend::new(FfmpegConfig { path: Some(script), hwaccel: false, ..Default::default() });
    let mut stream = video_core::demux::StreamInfo::new(1, StreamKind::Video, video_core::demux::Codec::H264);
    (stream.width, stream.height) = (320, 240);
    let mut dec = b.open_video_decoder(&stream, 1).unwrap();
    let start = Instant::now();
    let mut result = Ok(());
    for i in 0..1000u64 {
        let p = Packet { stream: 1, pts: Duration::from_millis(i * 33), keyframe: i == 0, data: vec![0; 100_000], generation: 0 };
        result = dec.send_packet(&p);
        if result.is_err() {
            break;
        }
    }
    let elapsed = start.elapsed();
    drop(dec);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(result.is_err(), "a stuck ffmpeg must surface as an error");
    assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
}

/// Opening and playing natively decodable media never runs ffmpeg at all (not even to probe it):
/// the "ffmpeg" here records every start in a marker file.
#[cfg(unix)]
#[test]
fn native_media_never_runs_ffmpeg() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("video-core-spy-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (script, marker) = (dir.join("ffmpeg"), dir.join("ran"));
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", marker.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = PlayerConfig {
        decoder_threads: 2,
        audio_output: AudioOutputConfig::Null(NullOutput::new(RATE, 2)),
        ffmpeg: FfmpegConfig { path: Some(script), ..Default::default() },
        ..Default::default()
    };
    // VP9 video + Opus audio, both native.
    for name in ["vp9_profile0.webm", "av1_with_audio.webm"] {
        let player = Player::open(Source::parse(&fixture_path(name)).unwrap(), config.clone()).unwrap();
        until("first frame", 10, || player.current_frame());
    }
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ran.is_empty(), "ffmpeg was run: {ran}");
}
