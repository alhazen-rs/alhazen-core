//! Windows' own decoders through Media Foundation. Runs on Windows only; a test whose decoder is
//! not installed (e.g. HEVC without the Store extension, or on Windows Server) prints a note and
//! passes.
#![cfg(all(windows, feature = "media-foundation", feature = "native"))]

use std::time::{Duration, Instant};

use alhazen_core::audio::{AudioOutputConfig, NullOutput};
use alhazen_core::backend::Registry;
use alhazen_core::decode::{AudioDecoder, DecodedFrame, VideoDecoder};
use alhazen_core::demux::{Demuxer, StreamInfo, StreamKind};
use alhazen_core::hw::select::HwCodec;
use alhazen_core::mf::{MfAudioDecoder, MfVideoDecoder};
use alhazen_core::{FfmpegConfig, Player, PlayerConfig, PlayerState, Source};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn demuxer(name: &str) -> Box<dyn Demuxer> {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
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
    let codec = HwCodec::of(&s).unwrap();
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
    let codec = HwCodec::of(&s).unwrap();
    let mut dec = MfAudioDecoder::new(codec, &s).unwrap();
    let (mut frames, mut rate, mut channels, mut peak) = (0, 0, 0, 0f32);
    let mut take = |b: alhazen_core::decode::AudioBuffer| {
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

/// Runs `f(name)` on its own thread and fails the test, naming the file, if it hangs.
fn within_30s<T: Send + 'static>(name: &'static str, f: fn(&str) -> T) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(f(name)));
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("{name}: still decoding after 30 s"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("{name}: decoding panicked"),
    }
}

#[test]
fn audio_codecs_windows_ships() {
    // 0.5 s of a 440 Hz tone at 48 kHz stereo (AAC: 1 s at 44.1 kHz mono), within codec padding.
    for (name, expect, rate, channels) in [
        ("mp3.mkv", 24_000, 48_000, 2),
        ("mp3.mp4", 24_000, 48_000, 2),
        ("ac3.mkv", 24_000, 48_000, 2),
        ("eac3.mkv", 24_000, 48_000, 2),
        ("alac.m4a", 24_000, 48_000, 2),
        ("h264_aac.mp4", 44_100, 44_100, 1),
    ] {
        let Some((frames, r, c, peak)) = within_30s(name, decode_audio) else { continue };
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

/// AAC through Media Foundation lines up with ffmpeg (which honours the MP4 edit list): the
/// encoder's start-up padding is trimmed.
#[test]
fn aac_padding_is_trimmed() {
    let Some(reference) = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture("h264_aac.mp4"), "-map", "0:a:0", "-f", "f32le", "-"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect::<Vec<f32>>())
    else {
        eprintln!("skipped: no ffmpeg");
        return;
    };
    let mut d = demuxer("h264_aac.mp4");
    let s = track(d.as_ref(), StreamKind::Audio);
    let mut dec = MfAudioDecoder::new(HwCodec::of(&s).unwrap(), &s).unwrap();
    let (mut ours, mut first) = (vec![], None);
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        dec.send_packet(&p).unwrap();
        while let Some(b) = dec.receive_samples().unwrap() {
            first.get_or_insert(b.pts);
            ours.extend(b.samples);
        }
    }
    assert_eq!(first, Some(Duration::ZERO));
    let n = ours.len().min(reference.len()).min(44_100);
    let worst = ours[..n].iter().zip(&reference[..n]).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst < 0.01, "not aligned with ffmpeg: worst {worst}");
}

#[test]
fn ten_bit_hevc_keeps_its_samples() {
    let mut d = demuxer("hdr_pq.mkv");
    let s = track(d.as_ref(), StreamKind::Video);
    let mut dec = MfVideoDecoder::new(HwCodec::Hevc, &s, true).unwrap().with_ten_bit(true);
    let frame = loop {
        let Some(p) = d.next_packet().unwrap() else { panic!("no frame") };
        if p.stream != s.id {
            continue;
        }
        if let Err(e) = dec.send_packet(&p) {
            if e.to_string().contains("no Media Foundation decoder") {
                return eprintln!("skipped: {e}");
            }
            panic!("{e}");
        }
        if let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            break f;
        }
    };
    assert_eq!(frame.bit_depth, 10);
    let alhazen_core::decode::YuvPlanes::U16(p) = &frame.planes else { panic!("expected 16-bit planes") };
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture("hdr_pq.mkv"), "-frames:v", "1", "-pix_fmt", "yuv420p10le", "-f", "rawvideo", "-"])
        .output();
    let Ok(out) = out else { return eprintln!("no ffmpeg") };
    let reference: Vec<u16> = out.stdout.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
    assert!(p.iter().flatten().copied().eq(reference), "10-bit samples differ from ffmpeg's");
}

// GPU conversion (`OutputPrefs::gpu_bgra`): the D3D11 video processor against the CPU path.

/// The first frame of `name` through Media Foundation, once converted by the video processor
/// (with `hint`) and once as YUV for the CPU path; `None` when that can't run here.
fn processor_and_cpu(name: &str, hint: Option<(u32, u32)>) -> Option<(Vec<u8>, (u32, u32), alhazen_core::decode::YuvFrame)> {
    let mut d = demuxer(name);
    let s = track(d.as_ref(), StreamKind::Video);
    let codec = HwCodec::of(&s).unwrap();
    let mut gpu = MfVideoDecoder::new(codec, &s, true).unwrap().with_ten_bit(true).with_gpu_bgra(true);
    let mut cpu = MfVideoDecoder::new(codec, &s, true).unwrap().with_ten_bit(true);
    gpu.set_output_hint(hint);
    let (mut g, mut c) = (None, None);
    while g.is_none() || c.is_none() {
        let Some(p) = d.next_packet().unwrap() else { break };
        if p.stream != s.id {
            continue;
        }
        if let Err(e) = gpu.send_packet(&p).and_then(|()| cpu.send_packet(&p)) {
            if e.to_string().contains("no Media Foundation decoder") {
                eprintln!("skipped {name}: {e}");
                return None;
            }
            panic!("{name}: {e}");
        }
        g = g.or(gpu.receive_frame().unwrap());
        c = c.or(cpu.receive_frame().unwrap());
    }
    let mut y = match c {
        Some(DecodedFrame::Yuv(y)) => y,
        other => panic!("{name}: CPU path gave {other:?}"),
    };
    y.color.apply_stream(&alhazen_core::color::StreamColor::of(&s));
    match g {
        Some(DecodedFrame::Bgra { width, height, bgra, .. }) => Some((bgra.to_vec(), (width, height), y)),
        _ if gpu.gpu_fallback() || gpu.description().is_some_and(|(_, on_gpu)| !on_gpu) => {
            eprintln!("skipped {name}: no D3D11 video processor here");
            None
        }
        other => panic!("{name}: GPU path gave {other:?}"),
    }
}

/// The CPU path's full-size BGRA, scaled to `size` in RGB (see `tests/nvdec.rs`).
fn cpu_reference(y: &alhazen_core::decode::YuvFrame, size: (u32, u32)) -> Vec<u8> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let mut full = Vec::new();
    alhazen_core::convert::frame_to_bgra(y, &mut full).unwrap();
    if size == (y.width, y.height) {
        return full;
    }
    let src = ImageRef::new(y.width, y.height, &full, PixelType::U8x4).unwrap();
    let mut dst = Image::new(size.0, size.1, PixelType::U8x4);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    Resizer::new().resize(&src, &mut dst, &options).unwrap();
    dst.into_vec()
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

#[test]
fn sdr_video_processor_matches_cpu() {
    for hint in [None, Some((160, 90))] {
        let Some((bgra, size, y)) = processor_and_cpu("h264_aac.mp4", hint) else { return };
        let want = hint.and_then(|h| alhazen_core::scale::fit_within(y.width, y.height, h)).unwrap_or((y.width, y.height));
        assert_eq!(size, want);
        let db = psnr(&bgra, &cpu_reference(&y, size));
        eprintln!("h264 at {size:?}: {db:.1} dB");
        assert!(db >= if hint.is_none() { 45.0 } else { 35.0 }, "{db:.1} dB at {size:?}");
    }
}

#[test]
fn hdr_video_processor_tone_maps() {
    let Some((bgra, size, y)) = processor_and_cpu("hdr_pq.mkv", None) else { return };
    let reference = cpu_reference(&y, size);
    let db = psnr(&bgra, &reference);
    // Drivers tone map their own way: close to ours, and black and white where they should be.
    let luma = |p: &[u8]| (p[2] as u32 * 2126 + p[1] as u32 * 7152 + p[0] as u32 * 722) / 10000;
    let (lo, hi) = bgra.chunks(4).map(luma).fold((255, 0), |(lo, hi), l| (lo.min(l), hi.max(l)));
    eprintln!("hdr_pq: {db:.1} dB, luma {lo}..{hi}");
    assert!(db >= 30.0, "{db:.1} dB");
    assert!(lo <= 8 && hi >= 240, "black {lo}, white {hi}");
}

#[test]
fn blit_failure_falls_back_to_cpu_tone_mapped() {
    // The 3rd blit fails on purpose (debug builds): playback goes on through the CPU path.
    // SAFETY: test-only switch, read when the video processor is created.
    unsafe { std::env::set_var("ALHAZEN_TEST_BREAK_VPBLT", "3") };
    let mut d = demuxer("hdr_pq.mkv");
    let s = track(d.as_ref(), StreamKind::Video);
    let mut dec = MfVideoDecoder::new(HwCodec::Hevc, &s, true).unwrap().with_ten_bit(true).with_gpu_bgra(true);
    let mut kinds = Vec::new();
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        if let Err(e) = dec.send_packet(&p) {
            unsafe { std::env::remove_var("ALHAZEN_TEST_BREAK_VPBLT") };
            if e.to_string().contains("no Media Foundation decoder") {
                return eprintln!("skipped: {e}");
            }
            panic!("{e}");
        }
        while let Some(f) = dec.receive_frame().unwrap() {
            kinds.push(matches!(f, DecodedFrame::Bgra { .. }));
        }
    }
    unsafe { std::env::remove_var("ALHAZEN_TEST_BREAK_VPBLT") };
    if kinds.first() != Some(&true) {
        return eprintln!("skipped: no D3D11 video processor here");
    }
    assert_eq!(&kinds[..3], &[true, true, false], "two blits, then the CPU path");
    assert!(kinds[2..].iter().all(|&gpu| !gpu), "stays on the CPU path");
    assert!(dec.gpu_fallback());
    assert!(kinds.len() >= 40, "{} frames", kinds.len());
}
