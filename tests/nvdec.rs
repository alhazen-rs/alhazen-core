//! NVIDIA's GPU decoders (NVDEC) on Linux. Each test prints a note and passes on machines without
//! NVDEC (CI); on an NVIDIA machine they all run.
#![cfg(all(target_os = "linux", feature = "nvdec", feature = "native"))]

use std::process::Command;
use std::time::Duration;

use alhazen_core::backend::Registry;
use alhazen_core::decode::{DecodedFrame, VideoDecoder, YuvFrame};
use alhazen_core::demux::{Demuxer, StreamInfo, StreamKind};
use alhazen_core::nvdec::NvdecVideoDecoder;
use alhazen_core::Source;

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

fn gpu() -> bool {
    let ok = alhazen_core::nvdec::available();
    if !ok {
        eprintln!("skipped: no NVDEC on this machine");
    }
    ok
}

/// Every frame of `name` through NVDEC, with an optional output hint.
fn decode(name: &str, hint: Option<(u32, u32)>) -> Vec<YuvFrame> {
    let mut d = demuxer(name);
    let s = video(d.as_ref());
    let mut dec = NvdecVideoDecoder::new(&s).unwrap();
    let mut frames = vec![];
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        dec.set_output_hint(hint);
        dec.send_packet(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            frames.push(f);
        }
    }
    dec.send_eof();
    while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
        frames.push(f);
    }
    frames
}

fn check(name: &str, count: usize) {
    let frames = decode(name, None);
    let pts: Vec<Duration> = frames.iter().map(|f| f.pts).collect();
    assert_eq!(frames.len(), count, "{name}: frames");
    assert!(frames.iter().all(|f| (f.width, f.height) == (320, 240)), "{name}: size");
    assert_eq!(pts[0], Duration::ZERO, "{name}: first pts");
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "{name}: presentation order");
}

#[test]
fn every_codec_decodes_every_frame() {
    if !gpu() {
        return;
    }
    check("h264_aac.mp4", 30);
    check("hevc.mkv", 30);
    check("hevc_10bit.mp4", 30);
    check("vp8.webm", 60);
    check("vp9_profile0.webm", 60);
    check("vp9_10bit.webm", 60);
    check("vp9.mp4", 60);
    check("av1.webm", 60);
    check("av1.mp4", 60);
}

/// ffmpeg's first frame as 8-bit planar 4:2:0, or `None` without ffmpeg.
fn ffmpeg_first_frame(name: &str) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-frames:v", "1", "-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
        .output()
        .ok()?;
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

#[test]
fn pictures_match_ffmpeg() {
    if !gpu() {
        return;
    }
    for (name, min_db) in [
        ("h264_aac.mp4", 45.0),
        ("hevc.mkv", 45.0),
        ("vp8.webm", 45.0),
        ("vp9_profile0.webm", 45.0),
        ("av1.webm", 45.0),
        // 10-bit: we keep the high 8 bits, ffmpeg rounds/dithers; still visually identical.
        ("hevc_10bit.mp4", 35.0),
        ("vp9_10bit.webm", 35.0),
    ] {
        let f = &decode(name, None)[0];
        let ours: Vec<u8> = f.planes.concat();
        let Some(reference) = ffmpeg_first_frame(name) else {
            eprintln!("skipped comparison: no ffmpeg");
            return;
        };
        let db = psnr(&ours, &reference);
        eprintln!("{name}: {db:.1} dB");
        assert!(db >= min_db, "{name}: {db:.1} dB");
    }
}

#[test]
fn resolution_change_mid_stream() {
    if !gpu() {
        return;
    }
    let frames = decode("vp9_size_change.webm", None);
    let small = frames.iter().filter(|f| (f.width, f.height) == (320, 240)).count();
    let large = frames.iter().filter(|f| (f.width, f.height) == (640, 360)).count();
    assert_eq!((small, large), (30, 30));
    assert!(frames[..30].iter().all(|f| f.width == 320), "small frames first");
}

#[test]
fn dropping_mid_stream_then_decoding_again() {
    if !gpu() {
        return;
    }
    let mut d = demuxer("hevc.mkv");
    let s = video(d.as_ref());
    let mut dec = NvdecVideoDecoder::new(&s).unwrap();
    for _ in 0..5 {
        let p = d.next_packet().unwrap().unwrap();
        dec.send_packet(&p).unwrap();
    }
    drop(dec);
    check("hevc.mkv", 30);
}

#[test]
fn two_decoders_on_two_threads() {
    if !gpu() {
        return;
    }
    let a = std::thread::spawn(|| decode("av1.webm", None).len());
    let b = std::thread::spawn(|| decode("h264_aac.mp4", None).len());
    assert_eq!((a.join().unwrap(), b.join().unwrap()), (60, 30));
}

#[test]
fn flush_then_restart_from_the_first_keyframe() {
    if !gpu() {
        return;
    }
    let mut d = demuxer("h264_aac.mp4");
    let s = video(d.as_ref());
    let mut dec = NvdecVideoDecoder::new(&s).unwrap();
    let packets: Vec<_> = std::iter::from_fn(|| d.next_packet().unwrap()).filter(|p| p.stream == s.id).collect();
    for p in &packets[..10] {
        dec.send_packet(p).unwrap();
    }
    dec.flush();
    assert!(dec.receive_frame().unwrap().is_none(), "flush drops queued frames");
    let mut pts = vec![];
    for p in &packets {
        dec.send_packet(p).unwrap();
        while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            pts.push(f.pts);
        }
    }
    dec.send_eof();
    while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
        pts.push(f.pts);
    }
    assert_eq!(pts.len(), 30);
    assert_eq!(pts[0], Duration::ZERO);
}

#[test]
fn streams_outside_the_gpus_size_limits_are_not_claimed() {
    if !gpu() {
        return;
    }
    use alhazen_core::backend::Backend;
    use alhazen_core::demux::Codec;
    let backend = alhazen_core::nvdec::NvdecBackend::new(true);
    let stream = |codec, w, h| {
        let mut s = StreamInfo::new(1, StreamKind::Video, codec);
        (s.width, s.height) = (w, h);
        s
    };
    assert!(backend.supports_video(&stream(Codec::Hevc, 1920, 1080)));
    assert!(!backend.supports_video(&stream(Codec::Hevc, 64, 64)), "below HEVC's 144x144 minimum");
    assert!(!backend.supports_video(&stream(Codec::H264, 8192, 8192)), "above H.264's 4096x4096 maximum");
    assert!(backend.supports_video(&stream(Codec::Av1, 0, 0)), "unknown size: let the decoder decide");
}
