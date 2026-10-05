//! Windows' own decoders through Media Foundation. Runs on Windows only; a test whose decoder is
//! not installed (e.g. HEVC without the Store extension, or on Windows Server) prints a note and
//! passes.
#![cfg(all(windows, feature = "media-foundation", feature = "native"))]

use std::time::{Duration, Instant};

use video_core::audio::{AudioOutputConfig, NullOutput};
use video_core::backend::Registry;
use video_core::decode::{AudioDecoder, DecodedFrame, VideoDecoder};
use video_core::demux::{Demuxer, StreamInfo, StreamKind};
use video_core::mf::select::MfCodec;
use video_core::mf::{MfAudioDecoder, MfVideoDecoder};
use video_core::{FfmpegConfig, Player, PlayerConfig, PlayerState, Source};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn demuxer(name: &str) -> Box<dyn Demuxer> {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = video_core::demux::probe(src.as_mut()).unwrap().unwrap();
    Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap()
}

fn track(d: &dyn Demuxer, kind: StreamKind) -> StreamInfo {
    d.streams().iter().find(|s| s.kind == kind).unwrap().clone()
}

/// Every frame of `name`'s video through Media Foundation: (pts list, size), or `None` when no
/// decoder is installed.
fn decode_video(name: &str) -> Option<(Vec<Duration>, (u32, u32), String)> {
    let mut d = demuxer(name);
    let s = track(d.as_ref(), StreamKind::Video);
    let codec = MfCodec::of(&s).unwrap();
    let mut dec = MfVideoDecoder::new(codec, &s, true).unwrap();
    let (mut pts, mut size) = (vec![], (0, 0));
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        if let Err(e) = dec.send_packet(&p) {
            if pts.is_empty() && e.to_string().contains("no Media Foundation decoder") {
                eprintln!("skipped {name}: {e}");
                return None;
            }
            panic!("{name}: {e}");
        }
        while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            size = (f.width, f.height);
            pts.push(f.pts);
        }
    }
    dec.send_eof();
    while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
        pts.push(f.pts);
    }
    let (name, gpu) = dec.description().unwrap();
    Some((pts, size, format!("{name} ({})", if gpu { "GPU" } else { "software" })))
}

fn check_video(name: &str, frames: usize, size: (u32, u32)) {
    let Some((pts, got, decoder)) = decode_video(name) else { return };
    eprintln!("{name}: {decoder}");
    assert_eq!(pts.len(), frames, "{name}: frames");
    assert_eq!(got, size, "{name}: size");
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "{name}: presentation order {pts:?}");
    assert_eq!(pts[0], Duration::ZERO, "{name}: first pts");
}

#[test]
fn h264() {
    check_video("h264_aac.mp4", 30, (320, 240));
}

#[test]
fn hevc_8_and_10_bit() {
    check_video("hevc.mkv", 30, (320, 240));
    check_video("hevc_10bit.mp4", 30, (320, 240));
}

#[test]
fn vp9_and_av1() {
    check_video("vp9_profile0.webm", 60, (320, 240));
    check_video("av1.webm", 60, (320, 240));
}

/// All samples of `name`'s audio through Media Foundation: (frames, rate, channels, peak), or
/// `None` when no decoder is installed.
fn decode_audio(name: &str) -> Option<(usize, u32, u16, f32)> {
    let mut d = demuxer(name);
    let s = track(d.as_ref(), StreamKind::Audio);
    let codec = MfCodec::of(&s).unwrap();
    let mut dec = MfAudioDecoder::new(codec, &s).unwrap();
    let (mut frames, mut rate, mut channels, mut peak) = (0, 0, 0, 0f32);
    let mut take = |b: video_core::decode::AudioBuffer| {
        (rate, channels) = (b.rate, b.channels);
        frames += b.samples.len() / b.channels as usize;
        peak = b.samples.iter().fold(peak, |m, s| m.max(s.abs()));
    };
    let mut any = false;
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        if let Err(e) = dec.send_packet(&p) {
            if !any && e.to_string().contains("no Media Foundation decoder") {
                eprintln!("skipped {name}: {e}");
                return None;
            }
            panic!("{name}: {e}");
        }
        any = true;
        while let Some(b) = dec.receive_samples().unwrap() {
            take(b);
        }
    }
    dec.send_eof();
    while let Some(b) = dec.receive_samples().unwrap() {
        take(b);
    }
    Some((frames, rate, channels, peak))
}

#[test]
fn audio_codecs_windows_ships() {
    // 0.5 s of a 440 Hz tone at 48 kHz stereo (AAC: 1 s at 44.1 kHz mono), within codec padding.
    for (name, expect, rate, channels) in [
        ("mp3.mkv", 24_000, 48_000, 2),
        ("mp3.mp4", 24_000, 48_000, 2),
        ("ac3.mkv", 24_000, 48_000, 2),
        ("eac3.mkv", 24_000, 48_000, 2),
        ("flac.mkv", 24_000, 48_000, 2),
        ("alac.m4a", 24_000, 48_000, 2),
        ("h264_aac.mp4", 44_100, 44_100, 1),
    ] {
        let Some((frames, r, c, peak)) = decode_audio(name) else { continue };
        eprintln!("{name}: {frames} frames, {r} Hz, {c} ch, peak {peak:.3}");
        assert_eq!((r, c), (rate, channels), "{name}");
        assert!(frames.abs_diff(expect) <= 2 * 1152 + 2048, "{name}: {frames} frames, expected about {expect}");
        assert!(peak > 0.05, "{name}: silent");
    }
}

/// The phase goal: H.264 + AAC plays with sound, in sync, with ffmpeg turned off.
#[test]
fn h264_aac_plays_in_sync_without_ffmpeg() {
    if decode_video("h264_aac.mp4").is_none() || decode_audio("h264_aac.mp4").is_none() {
        return;
    }
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig {
        audio_output: AudioOutputConfig::Null(null.clone()),
        ffmpeg: FfmpegConfig { enabled: false, ..Default::default() },
        ..Default::default()
    };
    let player = Player::open(Source::parse(&fixture("h264_aac.mp4")).unwrap(), config).unwrap();
    assert!(player.has_video() && player.has_audio());
    assert_eq!(player.stats().video_backend, Some("media-foundation"));
    player.play();
    let start = Instant::now();
    let (mut pulled, mut checked) = (0u64, 0);
    while player.state() != PlayerState::Ended {
        assert!(start.elapsed() < Duration::from_secs(20), "never ended");
        let due = (start.elapsed().as_secs_f64() * 48_000.0) as u64;
        null.pull((due - pulled) as usize);
        pulled = due;
        if let Some(f) = player.current_frame() {
            let pos = player.position();
            // Steady state, after a 300 ms warm-up.
            if pos > Duration::from_millis(300) && pos < Duration::from_millis(900) {
                assert!(pos.abs_diff(f.pts()) <= Duration::from_millis(45), "A/V offset at {pos:?}");
                checked += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(checked > 20, "only {checked} sync checks");
}

#[test]
fn seeking_restarts_the_decoder_at_the_target() {
    if decode_video("h264_aac.mp4").is_none() {
        return;
    }
    let config = PlayerConfig {
        audio_output: AudioOutputConfig::Disabled,
        ffmpeg: FfmpegConfig { enabled: false, ..Default::default() },
        ..Default::default()
    };
    let player = Player::open(Source::parse(&fixture("h264_aac.mp4")).unwrap(), config).unwrap();
    let wait = |what: &str, f: &mut dyn FnMut() -> bool| {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    wait("first frame", &mut || player.current_frame().is_some());
    for target in [700u64, 200, 500] {
        let t = Duration::from_millis(target);
        player.seek(t);
        wait("frame at target", &mut || {
            player.current_frame().is_some_and(|f| f.pts() <= t && f.pts() + Duration::from_millis(34) > t)
        });
    }
}
