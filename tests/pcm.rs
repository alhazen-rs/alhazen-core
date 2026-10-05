//! Uncompressed PCM audio (QuickTime `in24`/`twos`/`sowt`/`fl32`/`raw `, ISO `ipcm`, Matroska
//! `A_PCM/*`): decoded natively, sample-exact against ffmpeg when it is installed.
#![cfg(feature = "native")]

use std::process::Command;
use std::time::{Duration, Instant};

use video_core::audio::{AudioOutputConfig, NullOutput};
use video_core::backend::Registry;
use video_core::demux::StreamKind;
use video_core::{Player, PlayerConfig, Source};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// All samples of the first audio track, decoded natively: (rate, channels, interleaved samples).
fn native(name: &str) -> (u32, u16, Vec<f32>) {
    let source = Source::parse(&fixture(name)).unwrap();
    let mut src = source.open().unwrap();
    let format = video_core::demux::probe(src.as_mut()).unwrap().unwrap();
    let registry = Registry::with_ffmpeg(&video_core::FfmpegConfig { enabled: false, ..Default::default() });
    let mut demuxer = registry.open_demuxer(&source, format, src, None).unwrap();
    let stream = demuxer.streams().iter().find(|s| s.kind == StreamKind::Audio).expect("an audio track").clone();
    let mut dec = registry.open_audio_decoder(&stream, None).unwrap();
    let (mut rate, mut channels, mut out) = (0, 0, vec![]);
    while let Some(p) = demuxer.next_packet().unwrap() {
        if p.stream != stream.id {
            continue;
        }
        dec.send_packet(&p).unwrap();
        while let Some(b) = dec.receive_samples().unwrap() {
            (rate, channels) = (b.rate, b.channels);
            out.extend(b.samples);
        }
    }
    (rate, channels, out)
}

/// ffmpeg's decode as f32, or `None` without ffmpeg.
fn ffmpeg(name: &str) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-map", "0:a:0", "-f", "f32le", "-"])
        .output()
        .ok()?;
    assert!(out.status.success());
    Some(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect())
}

fn check(name: &str) {
    let (rate, channels, ours) = native(name);
    assert_eq!((rate, channels), (48_000, 2), "{name}");
    assert_eq!(ours.len(), 12_000 * 2, "{name}: 0.25 s of stereo");
    let peak = ours.iter().fold(0f32, |m, s| m.max(s.abs()));
    // ffmpeg's sine is 1/8; `-ac 2` spread it over two channels at -3 dB (0.0884).
    assert!((0.08..0.1).contains(&peak), "{name}: peak {peak}");
    let Some(reference) = ffmpeg(name) else {
        eprintln!("skipped exact comparison for {name}: no ffmpeg");
        return;
    };
    assert_eq!(ours.len(), reference.len(), "{name}");
    let worst = ours.iter().zip(&reference).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(worst <= 1e-6, "{name}: differs from ffmpeg by up to {worst}");
}

#[test]
fn quicktime_in24_little_endian() {
    check("pcm_s24le.mov");
}

#[test]
fn quicktime_in24_big_endian() {
    check("pcm_s24be.mov");
}

#[test]
fn quicktime_sowt_and_twos() {
    check("pcm_s16le.mov");
    check("pcm_s16be.mov");
}

#[test]
fn quicktime_fl32_and_raw() {
    check("pcm_f32le.mov");
    check("pcm_u8.mov");
}

#[test]
fn iso_mp4_ipcm() {
    check("pcm_s16le.mp4");
}

#[test]
fn matroska_pcm() {
    for name in ["pcm_s16le.mkv", "pcm_s24be.mkv", "pcm_f32le.mkv", "pcm_u8.mkv"] {
        check(name);
    }
}

/// The user-reported case: ProRes video with 24-bit PCM audio must make sound.
#[test]
fn prores_with_pcm_audio_plays_sound() {
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig { decoder_threads: 2, audio_output: AudioOutputConfig::Null(null.clone()), ..Default::default() };
    let player = Player::open(Source::parse(&fixture("prores_pcm.mov")).unwrap(), config).unwrap();
    assert!(player.has_video() && player.has_audio());
    player.play();
    let start = Instant::now();
    loop {
        let s = null.pull(480);
        if s.iter().any(|x| x.abs() > 0.05) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no sound");
        std::thread::sleep(Duration::from_millis(10));
    }
}
