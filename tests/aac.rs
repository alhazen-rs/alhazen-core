//! Native AAC (rusty_aac): exact against ffmpeg, aligned on the presentation timeline (start-up
//! padding trimmed), WAVE channel order, errors instead of panics. Comparisons skip without ffmpeg.
#![cfg(all(feature = "native", feature = "native-aac"))]

use std::process::Command;
use std::time::Duration;

use alhazen_core::backend::Registry;
use alhazen_core::decode::{AacAudioDecoder, AudioDecoder};
use alhazen_core::demux::{Demuxer, Packet, StreamInfo, StreamKind};
use alhazen_core::Source;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn audio(name: &str) -> (StreamInfo, Vec<Packet>) {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let mut d: Box<dyn Demuxer> = Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap();
    let s = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
    let packets = std::iter::from_fn(|| d.next_packet().unwrap()).filter(|p| p.stream == s.id).collect();
    (s, packets)
}

/// (rate, channels, first pts, interleaved samples).
fn decode(s: &StreamInfo, packets: &[Packet]) -> (u32, u16, Duration, Vec<f32>) {
    let mut dec = AacAudioDecoder::new(s).unwrap();
    let (mut rate, mut ch, mut first, mut out) = (0, 0, None, vec![]);
    for p in packets {
        dec.send_packet(p).unwrap();
        while let Some(b) = dec.receive_samples().unwrap() {
            (rate, ch) = (b.rate, b.channels);
            first.get_or_insert(b.pts);
            out.extend(b.samples);
        }
    }
    (rate, ch, first.unwrap(), out)
}

fn ffmpeg(name: &str) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg").args(["-v", "error", "-i", &fixture(name), "-map", "0:a:0", "-f", "f32le", "-"]).output().ok()?;
    assert!(out.status.success());
    Some(out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
}

fn snr_db(ours: &[f32], reference: &[f32]) -> f64 {
    let n = ours.len().min(reference.len());
    let sig: f64 = reference[..n].iter().map(|x| (*x as f64).powi(2)).sum();
    let noise: f64 = ours[..n].iter().zip(&reference[..n]).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
    10.0 * (sig / noise.max(1e-30)).log10()
}

#[test]
fn matches_ffmpeg_with_the_padding_trimmed() {
    for name in ["aac_only.m4a", "h264_aac.mp4", "av1_aac.mp4"] {
        let (s, packets) = audio(name);
        let (_, _, first, ours) = decode(&s, &packets);
        assert_eq!(first, Duration::ZERO, "{name}: presentation starts at 0");
        let Some(reference) = ffmpeg(name) else { return };
        // Offset 0: the same samples at the same positions as ffmpeg (which honours the edit list).
        let db = snr_db(&ours, &reference);
        eprintln!("{name}: {db:.1} dB, {} vs {} samples", ours.len(), reference.len());
        assert!(db >= 100.0, "{name}: {db:.1} dB");
        assert!(ours.len().abs_diff(reference.len()) <= 2048, "{name}: length {} vs {}", ours.len(), reference.len());
    }
}

#[test]
fn flush_then_restart_is_aligned() {
    let (s, packets) = audio("h264_aac.mp4");
    let mut dec = AacAudioDecoder::new(&s).unwrap();
    for p in &packets[..10] {
        dec.send_packet(p).unwrap();
        while dec.receive_samples().unwrap().is_some() {}
    }
    dec.flush();
    // Back to the start (pts 0): padding trimmed again, first sample at 0.
    let (_, _, first, again) = {
        let mut out = vec![];
        let mut first = None;
        for p in &packets {
            dec.send_packet(p).unwrap();
            while let Some(b) = dec.receive_samples().unwrap() {
                first.get_or_insert(b.pts);
                out.extend(b.samples);
            }
        }
        (0, 0, first.unwrap(), out)
    };
    assert_eq!(first, Duration::ZERO);
    let (_, _, _, fresh) = decode(&s, &packets);
    assert_eq!(again, fresh, "same output as a fresh decoder");
}

#[test]
fn aac_51_centre_is_wave_channel_2() {
    let (s, packets) = audio("aac_51_center.mp4");
    let (rate, ch, _, out) = decode(&s, &packets);
    assert_eq!((rate, ch), (48_000, 6));
    let mut energy = [0f32; 6];
    for f in out.as_chunks::<6>().0 {
        for (e, x) in energy.iter_mut().zip(f) {
            *e += x * x;
        }
    }
    let loudest = energy.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
    assert_eq!(loudest, 2, "centre must be WAVE channel 2: {energy:?}");
}

#[test]
fn garbage_is_an_error_not_a_panic() {
    let (s, packets) = audio("h264_aac.mp4");
    let mut dec = AacAudioDecoder::new(&s).unwrap();
    let mut bad = packets[5].clone();
    // Pseudo-random bytes (0xFF… would be a valid frame: its first element is "end of frame").
    bad.data = (0..300u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    assert!(dec.send_packet(&bad).is_err());
    let mut s2 = s.clone();
    s2.extradata = None;
    assert!(AacAudioDecoder::new(&s2).is_err(), "no AudioSpecificConfig");
}
