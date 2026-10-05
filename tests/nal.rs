//! Length-prefixed (MP4/Matroska) H.264/HEVC to Annex B, checked by decoding the result with
//! ffmpeg's raw-stream parsers (skipped with a note without ffmpeg).
#![cfg(feature = "native")]

use std::io::Write;
use std::process::{Command, Stdio};

use video_core::demux::{Demuxer, Mp4Demuxer, StreamKind};
use video_core::nal::{AnnexB, ParamSetFormat};
use video_core::source::FileSource;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The video track's packets converted to an Annex B elementary stream.
fn annexb_stream(name: &str, format: ParamSetFormat) -> (Vec<u8>, usize) {
    let mut d = Mp4Demuxer::open(Box::new(FileSource::open(fixture(name)).unwrap())).unwrap();
    let s = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    let conv = AnnexB::from_config(format, s.extradata.as_deref().unwrap()).expect("parameter sets");
    let (mut out, mut packets) = (Vec::new(), 0);
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream == s.id {
            conv.convert(&p.data, p.keyframe, &mut out).unwrap();
            packets += 1;
        }
    }
    (out, packets)
}

/// Frames ffmpeg decodes from a raw elementary stream, or `None` without ffmpeg.
fn ffmpeg_frames(stream: &[u8], format: &str) -> Option<usize> {
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-f", format, "-i", "-", "-f", "framemd5", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take().unwrap();
    let data = stream.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).count())
}

#[test]
fn h264_from_mp4_becomes_a_decodable_annexb_stream() {
    let (stream, packets) = annexb_stream("h264_aac.mp4", ParamSetFormat::Avcc);
    assert_eq!(&stream[..4], &[0, 0, 0, 1], "starts with a start code");
    let Some(frames) = ffmpeg_frames(&stream, "h264") else { return eprintln!("skipped: no ffmpeg") };
    assert_eq!(frames, packets);
}

#[test]
fn hevc_from_mp4_becomes_a_decodable_annexb_stream() {
    let (stream, packets) = annexb_stream("hevc_10bit.mp4", ParamSetFormat::Hvcc);
    let Some(frames) = ffmpeg_frames(&stream, "hevc") else { return eprintln!("skipped: no ffmpeg") };
    assert_eq!(frames, packets);
}
