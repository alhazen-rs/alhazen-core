//! Content sync: a white flash and a beep start at the same moment; decoded, they must appear at
//! the same presentation time on every audio route (native, ffmpeg) and in every container,
//! including MP4s cut without re-encoding (edit lists skipping seconds, not just priming).
#![cfg(all(feature = "native", feature = "native-aac"))]

use std::time::Duration;

use alhazen_core::backend::{Backend, Registry};
use alhazen_core::decode::{AudioDecoder, DecodedFrame, VideoDecoder};
use alhazen_core::demux::{Demuxer, Packet, StreamInfo, StreamKind};
use alhazen_core::Source;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn open(name: &str) -> (Vec<StreamInfo>, Vec<Packet>) {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let mut d: Box<dyn Demuxer> = Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap();
    let streams = d.streams().to_vec();
    (streams, std::iter::from_fn(|| d.next_packet().unwrap()).collect())
}

fn stream(streams: &[StreamInfo], kind: StreamKind) -> StreamInfo {
    streams.iter().find(|s| s.kind == kind).unwrap().clone()
}

/// Presentation time of the first bright frame.
fn flash(name: &str) -> Duration {
    let (streams, packets) = open(name);
    let v = stream(&streams, StreamKind::Video);
    let mut dec = Registry::empty_with_native().open_video_decoder(&v, 2, None).unwrap();
    let mut bright = None;
    let mut take = |f: DecodedFrame| {
        let DecodedFrame::Yuv(f) = f else { return };
        let mean = f.planes[0].iter().map(|&y| y as u64).sum::<u64>() / f.planes[0].len() as u64;
        if mean > 128 && bright.is_none_or(|b| f.pts < b) {
            bright = Some(f.pts);
        }
    };
    for p in packets.iter().filter(|p| p.stream == v.id) {
        dec.send_packet(p).unwrap();
        while let Some(f) = dec.receive_frame().unwrap() {
            take(f);
        }
    }
    dec.send_eof();
    while let Some(f) = dec.receive_frame().unwrap() {
        take(f);
    }
    bright.expect("a white frame")
}

/// Presentation time of the beep's onset (first sample above half its peak).
fn beep(name: &str, mut dec: Box<dyn AudioDecoder>, a: &StreamInfo, packets: &[Packet]) -> Duration {
    let mut bufs = vec![];
    for p in packets.iter().filter(|p| p.stream == a.id) {
        dec.send_packet(p).unwrap();
        while let Some(b) = dec.receive_samples().unwrap() {
            bufs.push(b);
        }
    }
    dec.send_eof();
    while let Some(b) = dec.receive_samples().unwrap() {
        bufs.push(b);
    }
    let peak = bufs.iter().flat_map(|b| b.samples.iter()).fold(0f32, |m, s| m.max(s.abs()));
    for b in &bufs {
        let ch = b.channels as usize;
        if let Some(i) = b.samples.chunks(ch).position(|f| f.iter().any(|s| s.abs() > peak / 2.0)) {
            return b.pts + Duration::from_secs_f64(i as f64 / b.rate as f64);
        }
    }
    panic!("{name}: no beep");
}

fn check(name: &str, expected: Duration) {
    let v = flash(name);
    let (streams, packets) = open(name);
    let a = stream(&streams, StreamKind::Audio);
    let native = beep(name, Registry::empty_with_native().open_audio_decoder(&a, None).unwrap(), &a, &packets);
    eprintln!("{name}: flash {v:?}, beep (native) {native:?}");
    assert!(v.abs_diff(expected) <= Duration::from_millis(1), "{name}: flash at {v:?}, expected {expected:?}");
    assert!(native.abs_diff(v) <= Duration::from_millis(10), "{name}: native audio {native:?} vs video {v:?}");
    let ffmpeg = alhazen_core::ffmpeg::FfmpegCliBackend::new(Default::default());
    if ffmpeg.supports_audio(&a) {
        let via_ffmpeg = beep(name, ffmpeg.open_audio_decoder(&a).unwrap(), &a, &packets);
        eprintln!("{name}: beep (ffmpeg) {via_ffmpeg:?}");
        assert!(via_ffmpeg.abs_diff(v) <= Duration::from_millis(10), "{name}: ffmpeg audio {via_ffmpeg:?} vs video {v:?}");
    } else {
        eprintln!("skipped the ffmpeg route: no ffmpeg");
    }
}

#[test]
fn flash_and_beep_line_up_in_mp4() {
    check("sync.mp4", Duration::from_secs(1));
}

/// Both tracks' edit lists skip the first 0.5 s. We don't apply edit lists to video (timestamps
/// can't go negative), so both tracks keep that half second as a lead-in, in sync, as before;
/// only the audio's priming beyond the video's skip is trimmed.
#[test]
fn flash_and_beep_line_up_in_a_stream_copy_cut_mp4() {
    check("sync_cut.mp4", Duration::from_secs(1));
}

#[test]
fn flash_and_beep_line_up_in_matroska() {
    check("sync.mkv", Duration::from_secs(1));
}
