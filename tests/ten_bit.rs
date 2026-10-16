//! 10-bit output (`FrameFormat::Yuv10`): decoders keep their samples instead of shifting them
//! to 8 bits. Compared with ffmpeg's `yuv420p10le` decode when ffmpeg is on PATH.
#![cfg(feature = "native")]

use std::process::Command;

use alhazen_core::backend::Registry;
use alhazen_core::decode::{DecodedFrame, YuvFrame, YuvPlanes};
use alhazen_core::demux::StreamKind;
use alhazen_core::{OutputPrefs, Source};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The first frame from the native decoders, opened with `prefs`.
fn first_frame_with(name: &str, prefs: OutputPrefs) -> YuvFrame {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let registry = Registry::empty_with_native();
    let mut demuxer = registry.open_demuxer(&source, format, src, None).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    let mut dec = registry.open_video_decoder(&stream, 2, prefs, None).unwrap();
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

/// ffmpeg's first frame as 16-bit little-endian planar 4:2:0, or `None` without ffmpeg.
fn ffmpeg_10bit(name: &str) -> Option<Vec<u16>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-frames:v", "1", "-pix_fmt", "yuv420p10le", "-f", "rawvideo", "-"])
        .output()
        .ok()?;
    out.status.success().then(|| out.stdout.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect())
}

/// Y, then U, then V, rows without padding.
fn pack_u16(f: &YuvFrame) -> Vec<u16> {
    let YuvPlanes::U16(p) = &f.planes else { panic!("expected 16-bit planes") };
    let (cw, ch) = f.chroma_size();
    let dims = [(f.width, f.height), (cw, ch), (cw, ch)];
    let mut out = Vec::new();
    for (i, (w, h)) in dims.into_iter().enumerate() {
        for row in p[i].chunks(f.strides[i]).take(h as usize) {
            out.extend_from_slice(&row[..w as usize]);
        }
    }
    out
}

fn ten_bit() -> OutputPrefs {
    OutputPrefs { ten_bit: true, ..Default::default() }
}

#[test]
fn av1_10bit_planes_match_ffmpeg_exactly() {
    let f = first_frame_with("hdr_pq_av1.mkv", ten_bit());
    assert_eq!(f.bit_depth, 10);
    let Some(reference) = ffmpeg_10bit("hdr_pq_av1.mkv") else { return eprintln!("no ffmpeg: comparison skipped") };
    assert_eq!(pack_u16(&f), reference);
}

#[test]
fn vp9_10bit_planes_match_ffmpeg_exactly() {
    let f = first_frame_with("vp9_10bit.webm", ten_bit());
    assert_eq!(f.bit_depth, 10);
    let Some(reference) = ffmpeg_10bit("vp9_10bit.webm") else { return eprintln!("no ffmpeg: comparison skipped") };
    assert_eq!(pack_u16(&f), reference);
}

#[test]
fn yuv10_on_8bit_source_gives_u8() {
    let f = first_frame_with("vp9.mp4", ten_bit());
    assert_eq!(f.bit_depth, 8);
    assert!(matches!(f.planes, YuvPlanes::U8(_)));
}

#[test]
fn without_ten_bit_a_10bit_source_is_shifted_to_8() {
    let f = first_frame_with("hdr_pq_av1.mkv", OutputPrefs::default());
    assert_eq!(f.bit_depth, 8);
    assert!(matches!(f.planes, YuvPlanes::U8(_)));
}
