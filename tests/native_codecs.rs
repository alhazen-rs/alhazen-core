//! Native VP9/VP8/ProRes pictures compared with ffmpeg's decode of the same frame (PSNR).
//! Needs `ffmpeg` on PATH; without it each test prints a note and passes.
#![cfg(feature = "native")]

use std::process::Command;

use video_core::Source;
use video_core::backend::Registry;
use video_core::decode::{DecodedFrame, PixelLayout, YuvFrame};
use video_core::demux::StreamKind;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The first frame decoded natively, through the same registry and demuxers the player uses.
fn native_first_frame(name: &str) -> YuvFrame {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = video_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let registry = Registry::with_defaults();
    let mut demuxer = registry.open_demuxer(&source, format, src, None).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    let mut dec = registry.open_video_decoder(&stream, 2, None).unwrap();
    while let Some(p) = demuxer.next_packet().unwrap() {
        if p.stream != stream.id {
            continue;
        }
        dec.send_packet(&p).unwrap();
        if let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            return f;
        }
    }
    panic!("{name}: no frame");
}

/// ffmpeg's first frame of `name` as raw 8-bit planar YUV in `pix_fmt`, or `None` without ffmpeg.
fn ffmpeg_first_frame(name: &str, pix_fmt: &str) -> Option<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-frames:v", "1", "-pix_fmt", pix_fmt, "-f", "rawvideo", "-"])
        .output()
        .ok()?;
    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout)
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

fn check(name: &str, layout: PixelLayout, pix_fmt: &str, min_db: f64) {
    let native = native_first_frame(name);
    assert_eq!(native.layout, layout, "{name}");
    let Some(reference) = ffmpeg_first_frame(name, pix_fmt) else {
        eprintln!("skipped {name}: ffmpeg not found on PATH");
        return;
    };
    let ours: Vec<u8> = native.planes.concat();
    let db = psnr(&ours, &reference);
    assert!(db >= min_db, "{name}: PSNR {db:.1} dB < {min_db} dB");
}

#[test]
fn vp9_profile0_matches_ffmpeg() {
    check("vp9_profile0.webm", PixelLayout::I420, "yuv420p", 40.0);
}

#[test]
fn vp9_10bit_matches_ffmpeg() {
    check("vp9_10bit.webm", PixelLayout::I420, "yuv420p", 40.0);
}

#[test]
fn vp9_in_mp4_matches_ffmpeg() {
    check("vp9.mp4", PixelLayout::I420, "yuv420p", 40.0);
}

#[test]
fn vp8_matches_ffmpeg() {
    check("vp8.webm", PixelLayout::I420, "yuv420p", 40.0);
}

#[test]
fn prores_422_matches_ffmpeg() {
    check("prores_hq.mov", PixelLayout::I422, "yuv422p", 35.0);
}

#[test]
fn prores_4444_matches_ffmpeg() {
    check("prores_4444.mov", PixelLayout::I444, "yuv444p", 35.0);
}

#[test]
fn prores_4444_interlaced_matches_ffmpeg() {
    check("prores_4444_interlaced.mov", PixelLayout::I444, "yuv444p", 35.0);
}
