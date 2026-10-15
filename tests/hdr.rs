//! HDR files through the player come out tone mapped (CPU path, native decoders).
#![cfg(feature = "native")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::backend::Registry;
use alhazen_core::clock::MockClock;
use alhazen_core::color::tonemap::{source_peak, tonemap_pixel};
use alhazen_core::decode::{DecodedFrame, YuvFrame, YuvPlanes};
use alhazen_core::demux::StreamKind;
use alhazen_core::{OutputPrefs, Player, PlayerConfig, Primaries, Source, Transfer, VideoFrame};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The first frame from the native decoders, 10-bit, with the stream's colour description.
fn first_10bit_frame(name: &str) -> YuvFrame {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let registry = Registry::empty_with_native();
    let mut demuxer = registry.open_demuxer(&source, format, src, None).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    let prefs = OutputPrefs { ten_bit: true, ..Default::default() };
    let mut dec = registry.open_video_decoder(&stream, 2, prefs, None).unwrap();
    while let Some(p) = demuxer.next_packet().unwrap() {
        if p.stream == stream.id {
            dec.send_packet(&p).unwrap();
            if let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
                return f;
            }
        }
    }
    panic!("{name}: no frame");
}

/// The f64 reference tone mapping of a limited-range BT.2020 10-bit 4:2:0 frame, as BGRA.
fn reference_bgra(f: &YuvFrame) -> Vec<u8> {
    let YuvPlanes::U16(p) = &f.planes else { panic!("expected 10-bit planes") };
    let peak = source_peak(f.color.hdr.as_ref());
    let mut out = Vec::with_capacity((f.width * f.height * 4) as usize);
    for row in 0..f.height as usize {
        for col in 0..f.width as usize {
            let y = p[0][row * f.strides[0] + col] as f64;
            let (u, v) = (p[1][row / 2 * f.strides[1] + col / 2] as f64, p[2][row / 2 * f.strides[2] + col / 2] as f64);
            let (yf, uf, vf) = ((y - 64.0) / 876.0, (u - 512.0) / 896.0, (v - 512.0) / 896.0);
            let rgb = [yf + 1.4746 * vf, yf - 0.164_553 * uf - 0.571_353 * vf, yf + 1.8814 * uf].map(|c| c.clamp(0.0, 1.0));
            let [r, g, b] = tonemap_pixel(rgb, f.color.transfer, f.color.primaries, peak).map(|c| (c * 255.0).round() as u8);
            out.extend_from_slice(&[b, g, r, 255]);
        }
    }
    out
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

#[test]
fn hdr_av1_plays_tone_mapped_on_the_cpu_path() {
    let config = PlayerConfig {
        clock: Some(Arc::new(MockClock::new())),
        decoder_threads: 2,
        audio_output: AudioOutputConfig::Disabled,
        registry: Some(Arc::new(Registry::empty_with_native())),
        ..Default::default()
    };
    let player = Player::open(Source::parse(&fixture("hdr_pq_av1.mkv")).unwrap(), config).unwrap();
    let start = Instant::now();
    let frame = loop {
        if let Some(f) = player.current_frame() {
            break f;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no frame");
        std::thread::sleep(Duration::from_millis(2));
    };
    let VideoFrame::Bgra { bgra, width, height, .. } = frame else { panic!("expected BGRA") };
    assert_eq!(player.stats().hdr, Transfer::Pq);
    assert_eq!(player.stats().conversion, "cpu");
    let yuv = first_10bit_frame("hdr_pq_av1.mkv");
    assert_eq!((yuv.color.transfer, yuv.color.primaries), (Transfer::Pq, Primaries::Bt2020));
    assert_eq!((width, height), (yuv.width, yuv.height));
    let db = psnr(&bgra, &reference_bgra(&yuv));
    assert!(db >= 40.0, "{db:.1} dB against the reference tone mapping");
}
