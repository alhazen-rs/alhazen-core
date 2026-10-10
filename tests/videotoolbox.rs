//! Apple's decoders (VideoToolbox, AudioToolbox) on macOS. Runs on the macOS CI runners.
#![cfg(all(target_os = "macos", feature = "videotoolbox", feature = "native"))]

use std::process::Command;
use std::time::{Duration, Instant};

use alhazen_core::apple::{AppleBackend, AtAudioDecoder, VtVideoDecoder};
use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::backend::{Backend, Registry};
use alhazen_core::decode::{AudioDecoder, DecodedFrame, VideoDecoder, YuvFrame};
use alhazen_core::demux::{Codec, Demuxer, StreamInfo, StreamKind};
use alhazen_core::{Player, PlayerConfig, PlayerState, Source};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn demuxer(name: &str) -> Box<dyn Demuxer> {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap()
}

fn video(d: &dyn Demuxer) -> StreamInfo {
    d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone()
}

/// Every frame of `name` through VideoToolbox; `hint(packet index)` sets the output hint.
fn decode_with(name: &str, hint: impl Fn(usize) -> Option<(u32, u32)>) -> (usize, Vec<YuvFrame>) {
    let mut d = demuxer(name);
    let s = video(d.as_ref());
    let mut dec = VtVideoDecoder::new(&s).unwrap();
    let (mut packets, mut frames) = (0, vec![]);
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        dec.set_output_hint(hint(packets));
        dec.send_packet(&p).unwrap_or_else(|e| panic!("{name} packet {packets}: {e}"));
        packets += 1;
        while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            frames.push(f);
        }
    }
    dec.send_eof();
    while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
        frames.push(f);
    }
    (packets, frames)
}

/// ffmpeg's frame `index` scaled to `size`, 8-bit planar 4:2:0, or `None` without ffmpeg.
fn ffmpeg_frame(name: &str, index: usize, size: (u32, u32)) -> Option<Vec<u8>> {
    let filter = format!("select=eq(n\\,{index}),scale={}:{}", size.0, size.1);
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-vf", &filter, "-frames:v", "1", "-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len()).max(1);
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / n as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

fn check_pictures(name: &str, frames: &[YuvFrame]) {
    for (i, f) in frames.iter().enumerate().step_by(7) {
        let Some(reference) = ffmpeg_frame(name, i, (f.width, f.height)) else {
            eprintln!("no ffmpeg: picture comparison skipped");
            return;
        };
        let db = psnr(&f.planes.concat(), &reference);
        assert!(db >= 30.0, "{name} frame {i} at {}x{}: {db:.1} dB", f.width, f.height);
    }
}

#[test]
fn h264_and_hevc_decode_every_frame_correctly() {
    for name in ["h264_aac.mp4", "h264_aac.ts", "hevc.mkv", "hevc_10bit.mp4"] {
        let s = video(demuxer(name).as_ref());
        let (packets, frames) = decode_with(name, |_| None);
        assert_eq!(frames.len(), packets, "{name}: every packet gives a frame");
        assert!(frames.iter().all(|f| (f.width, f.height) == (s.width, s.height)), "{name}: full size");
        assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts), "{name}: display order");
        check_pictures(name, &frames);
    }
}

#[test]
fn the_backend_takes_h264_and_hevc_and_leaves_vp8_and_aac() {
    let b = AppleBackend::new(true);
    assert_eq!(b.name(), "videotoolbox");
    let v = |c| StreamInfo::new(1, StreamKind::Video, c);
    assert!(b.supports_video(&v(Codec::H264)));
    assert!(b.supports_video(&v(Codec::Hevc)));
    assert!(!b.supports_video(&v(Codec::Vp8)));
    let mut aac = StreamInfo::new(2, StreamKind::Audio, Codec::Aac);
    aac.extradata = Some(vec![0x12, 0x10]);
    assert!(!b.supports_audio(&aac), "plain AAC stays native");
    assert!(b.supports_audio(&StreamInfo::new(2, StreamKind::Audio, Codec::Ac3)));
    assert!(!b.supports_audio(&StreamInfo::new(2, StreamKind::Audio, Codec::Mp3)));
}

#[test]
fn hardware_only_codecs_decode_when_claimed() {
    // AV1 (M3 and newer), VP9, ProRes: claimed only with hardware; then they must decode.
    for name in ["av1.mp4", "vp9_profile0.webm", "prores_hq.mov"] {
        let s = video(demuxer(name).as_ref());
        if !AppleBackend::new(true).supports_video(&s) {
            eprintln!("{name}: no hardware decoder on this Mac (native keeps it)");
            continue;
        }
        let (packets, frames) = decode_with(name, |_| None);
        assert_eq!(frames.len(), packets, "{name}");
        check_pictures(name, &frames);
    }
}

#[test]
fn output_size_changes_at_keyframes_with_intact_pictures() {
    // h264_aac.ts: keyframes at frames 0, 25, 50.
    let (_, frames) = decode_with("h264_aac.ts", |i| if (10..40).contains(&i) { Some((160, 160)) } else { None });
    let widths: Vec<u32> = frames.iter().map(|f| f.width).collect();
    for (i, &w) in widths.iter().enumerate() {
        let expected = if (25..50).contains(&i) { 160 } else { 320 };
        assert_eq!(w, expected, "frame {i}: {widths:?}");
    }
    check_pictures("h264_aac.ts", &frames);
}

#[test]
fn flush_then_decode_again_from_a_keyframe() {
    let mut d = demuxer("h264_aac.mp4");
    let s = video(d.as_ref());
    let mut dec = VtVideoDecoder::new(&s).unwrap();
    let packets: Vec<_> = std::iter::from_fn(|| d.next_packet().unwrap()).filter(|p| p.stream == s.id).collect();
    for p in &packets[..10] {
        dec.send_packet(p).unwrap();
    }
    dec.flush();
    assert!(dec.receive_frame().unwrap().is_none(), "frames from before the flush");
    let key = packets.iter().position(|p| p.keyframe).unwrap();
    for p in &packets[key..key + 5] {
        dec.send_packet(p).unwrap();
    }
    dec.send_eof();
    let mut n = 0;
    while let Some(DecodedFrame::Yuv(_)) = dec.receive_frame().unwrap() {
        n += 1;
    }
    assert_eq!(n, 5);
}

#[test]
fn the_player_plays_h264_through_videotoolbox() {
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, decoder_threads: 2, ..Default::default() };
    let player = Player::open(Source::parse(&fixture("h264_aac.mp4")).unwrap(), config).unwrap();
    assert_eq!(player.stats().video_backend, Some("videotoolbox"));
    player.play();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) && player.state() != PlayerState::Ended {
        assert!(!matches!(player.state(), PlayerState::Error(_)), "{:?}", player.state());
        player.current_frame();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(player.state(), PlayerState::Ended);
}

/// Every sample of the first audio stream of `name` through AudioToolbox: (rate, channels, samples).
fn decode_audio(name: &str) -> (u32, u16, Vec<f32>) {
    let mut d = demuxer(name);
    let s = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
    assert!(AppleBackend::new(true).supports_audio(&s), "{name}: claimed");
    let mut dec = AtAudioDecoder::new(&s).unwrap();
    let (mut rate, mut channels, mut out) = (0, 0, Vec::new());
    let mut take = |dec: &mut AtAudioDecoder| {
        while let Some(b) = dec.receive_samples().unwrap() {
            (rate, channels) = (b.rate, b.channels);
            out.extend_from_slice(&b.samples);
        }
    };
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream == s.id {
            dec.send_packet(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
            take(&mut dec);
        }
    }
    dec.send_eof();
    take(&mut dec);
    (rate, channels, out)
}

/// ffmpeg's decode as interleaved f32, or `None` without ffmpeg.
fn ffmpeg_audio(name: &str, rate: u32, channels: u16) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-vn", "-ac", &channels.to_string(), "-ar", &rate.to_string(), "-f", "f32le", "-"])
        .output()
        .ok()?;
    out.status.success().then(|| out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
}

fn snr(reference: &[f32], test: &[f32]) -> f64 {
    let n = reference.len().min(test.len());
    let signal: f64 = reference[..n].iter().map(|&x| (x as f64).powi(2)).sum();
    let noise: f64 = reference[..n].iter().zip(&test[..n]).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum();
    if noise == 0.0 { 200.0 } else { 10.0 * (signal / noise).log10() }
}

#[test]
fn alac_ac3_and_eac3_decode_like_ffmpeg() {
    for (name, min_db) in [("alac.m4a", 90.0), ("ac3.mkv", 30.0), ("eac3.mkv", 30.0)] {
        let (rate, channels, samples) = decode_audio(name);
        assert!(rate > 0 && channels > 0 && !samples.is_empty(), "{name}");
        let Some(reference) = ffmpeg_audio(name, rate, channels) else {
            eprintln!("no ffmpeg: comparison skipped");
            continue;
        };
        let frames = |v: &[f32]| v.len() / channels as usize;
        let diff = frames(&samples).abs_diff(frames(&reference));
        assert!(diff <= 1536, "{name}: {} frames vs ffmpeg's {}", frames(&samples), frames(&reference));
        let db = snr(&reference, &samples);
        assert!(db >= min_db, "{name}: {db:.1} dB against ffmpeg");
    }
}

#[test]
fn audio_flush_then_decode_again() {
    let mut d = demuxer("ac3.mkv");
    let s = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
    let packets: Vec<_> = std::iter::from_fn(|| d.next_packet().unwrap()).filter(|p| p.stream == s.id).collect();
    let mut dec = AtAudioDecoder::new(&s).unwrap();
    for p in &packets[..5] {
        dec.send_packet(p).unwrap();
    }
    dec.flush();
    assert!(dec.receive_samples().unwrap().is_none(), "nothing from before the flush");
    for p in &packets[10..15] {
        dec.send_packet(p).unwrap();
    }
    dec.send_eof();
    let mut frames = 0;
    let mut first_pts = None;
    while let Some(b) = dec.receive_samples().unwrap() {
        first_pts.get_or_insert(b.pts);
        frames += b.frames();
    }
    assert_eq!(first_pts, Some(packets[10].pts), "timed from the packet after the flush");
    assert!(frames >= 4 * 1536, "{frames}");
}
