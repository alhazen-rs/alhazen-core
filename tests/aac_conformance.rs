//! ISO/IEC 14496-26 HE-AAC (SBR) and HE-AACv2 (SBR+PS) conformance streams, decoded natively and
//! compared with their reference PCM to within 2 LSB (16-bit), FATE's tolerance. The vectors are
//! downloaded by `scripts/fetch_aac_vectors.py`; set `AAC_VECTORS_DIR` to run (CI does).
#![cfg(all(feature = "native", feature = "native-aac"))]

use alhazen_core::backend::Registry;
use alhazen_core::decode::{AacAudioDecoder, AudioDecoder};
use alhazen_core::demux::StreamKind;
use alhazen_core::Source;

fn decode(path: &str) -> (u32, u16, Vec<f32>) {
    let source = Source::parse(path).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let mut d = Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap();
    let s = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
    let mut dec = AacAudioDecoder::new(&s).unwrap();
    let (mut rate, mut ch, mut out) = (0, 0, vec![]);
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        dec.send_packet(&p).unwrap();
        while let Some(b) = dec.receive_samples().unwrap() {
            (rate, ch) = (b.rate, b.channels);
            out.extend(b.samples);
        }
    }
    (rate, ch, out)
}

#[test]
fn he_aac_and_he_aac_v2_match_the_iso_reference_pcm() {
    let Ok(dir) = std::env::var("AAC_VECTORS_DIR") else {
        eprintln!("skipped: set AAC_VECTORS_DIR (see scripts/fetch_aac_vectors.py)");
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    for (stream, reference, rate) in [
        ("al_sbr_cm_48_2.mp4", "al_sbr_hq_cm_48_2.s16", 48_000),
        ("al_sbr_ps_04_new.mp4", "al_sbr_ps_04_ur.s16", 32_000),
    ] {
        let (got_rate, ch, ours) = decode(dir.join(stream).to_str().unwrap());
        let reference: Vec<i16> =
            std::fs::read(dir.join(reference)).unwrap().as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        assert_eq!((got_rate, ch), (rate, 2), "{stream}");
        let n = ours.len().min(reference.len());
        assert!(n * 10 >= reference.len() * 9, "{stream}: {} samples vs reference {}", ours.len(), reference.len());
        let worst = ours[..n]
            .iter()
            .zip(&reference[..n])
            .map(|(a, b)| ((a * 32768.0).round().clamp(-32768.0, 32767.0) as i32 - *b as i32).abs())
            .max()
            .unwrap();
        eprintln!("{stream}: {n} samples, worst {worst} LSB");
        assert!(worst <= 2, "{stream}: {worst} LSB from the reference");
    }
}

/// HE-AAC with an MP4 edit list: a stream-copy cut at 1.0 s keeps the packet before the cut and
/// skips into it with the edit list's `media_time`. After trimming, our first sample is the
/// reference's sample at 1.0 s (the delay is in seconds; HE-AAC's output rate is twice the core's).
#[test]
fn he_aac_edit_list_skip_is_trimmed_at_the_output_rate() {
    let Ok(dir) = std::env::var("AAC_VECTORS_DIR") else {
        eprintln!("skipped: set AAC_VECTORS_DIR (see scripts/fetch_aac_vectors.py)");
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let cut = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("he_aac_cut.mp4");
    let ffmpeg = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-ss", "1.0", "-i"])
        .arg(dir.join("al_sbr_cm_48_2.mp4"))
        .args(["-c", "copy", "-t", "3"])
        .arg(&cut)
        .status();
    if !ffmpeg.is_ok_and(|s| s.success()) {
        eprintln!("skipped: no ffmpeg to cut the stream");
        return;
    }
    let source = Source::parse(cut.to_str().unwrap()).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let d = Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap();
    let delay = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().codec_delay;
    assert!(!delay.is_zero(), "the cut has an edit-list skip");

    let (rate, ch, ours) = decode(cut.to_str().unwrap());
    assert_eq!((rate, ch), (48_000, 2));
    let reference: Vec<f32> = std::fs::read(dir.join("al_sbr_hq_cm_48_2.s16"))
        .unwrap()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
        .collect();
    // Past the fresh decoder's warm-up, find where our output lines up with the reference.
    let (from, len, cut_at) = (8192 * 2, 4800 * 2, 48_000 * 2);
    let error = |off: isize| -> f32 {
        let r = &reference[(cut_at as isize + off * 2) as usize + from..][..len];
        ours[from..from + len].iter().zip(r).map(|(a, b)| (a - b).abs()).sum()
    };
    let best = (-2048..=2048).min_by(|&a, &b| error(a).total_cmp(&error(b))).unwrap();
    eprintln!("codec_delay {delay:?}, best offset {best} frames, error {}", error(best));
    assert_eq!(best, 0, "first sample is {best} frames off the cut point");
    assert!(error(0) / (len as f32) < 1e-3, "lined up but not matching: {}", error(0));
}
