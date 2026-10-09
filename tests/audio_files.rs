//! Audio files (WAV, FLAC, MP3, ADTS, Ogg) through the native readers and decoders: decoded like
//! ffmpeg (when installed) at offset 0 with the same length, seeking, tags, playing to the end.
#![cfg(feature = "native")]

use std::process::Command;
use std::time::{Duration, Instant};

use alhazen_core::audio::{AudioOutputConfig, NullOutput};
use alhazen_core::backend::Registry;
use alhazen_core::decode::AudioBuffer;
use alhazen_core::demux::{Demuxer, StreamInfo, StreamKind};
use alhazen_core::{FfmpegConfig, Player, PlayerConfig, PlayerState, Source};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn open_path(path: &str) -> Box<dyn Demuxer> {
    let source = Source::parse(path).unwrap();
    let mut src = source.open().unwrap();
    let format = alhazen_core::demux::probe(src.as_mut()).unwrap().expect("a known container");
    Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap()
}

fn open(name: &str) -> Box<dyn Demuxer> {
    open_path(&fixture(name))
}

fn audio(d: &dyn Demuxer) -> StreamInfo {
    d.streams().iter().find(|s| s.kind == StreamKind::Audio).expect("an audio stream").clone()
}

struct Decoded {
    rate: u32,
    channels: u16,
    /// Presentation time of the first sample (`Duration::MAX` when nothing was decoded).
    first_pts: Duration,
    samples: Vec<f32>,
}

impl Decoded {
    fn end(&self) -> Duration {
        let frames = self.samples.len() / self.channels.max(1) as usize;
        self.first_pts + Duration::from_secs_f64(frames as f64 / self.rate.max(1) as f64)
    }
}

/// Decodes `s` from `d`'s current position with a fresh native decoder until `stop` returns true
/// (checked after each packet) or the stream ends.
fn decode(d: &mut dyn Demuxer, s: &StreamInfo, mut stop: impl FnMut(&Decoded) -> bool) -> Decoded {
    let mut dec = Registry::empty_with_native().open_audio_decoder(s, None).unwrap();
    let mut out = Decoded { rate: 0, channels: 0, first_pts: Duration::MAX, samples: Vec::new() };
    let take = |out: &mut Decoded, b: AudioBuffer| {
        if out.first_pts == Duration::MAX {
            out.first_pts = b.pts;
        }
        (out.rate, out.channels) = (b.rate, b.channels);
        out.samples.extend(b.samples);
    };
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream != s.id {
            continue;
        }
        dec.send_packet(&p).unwrap();
        while let Some(b) = dec.receive_samples().unwrap() {
            take(&mut out, b);
        }
        if stop(&out) {
            return out;
        }
    }
    dec.send_eof();
    while let Some(b) = dec.receive_samples().unwrap() {
        take(&mut out, b);
    }
    out
}

fn decode_all(name: &str) -> (StreamInfo, Decoded) {
    let mut d = open(name);
    let s = audio(&*d);
    let out = decode(&mut *d, &s, |_| false);
    (s, out)
}

/// ffmpeg's decode of the first audio stream as interleaved f32, or `None` without ffmpeg.
fn ffmpeg(name: &str) -> Option<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", &fixture(name), "-map", "0:a:0", "-f", "f32le", "-"])
        .output()
        .ok()?;
    assert!(out.status.success(), "ffmpeg failed on {name}: {}", String::from_utf8_lossy(&out.stderr));
    Some(out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
}

fn snr_db(ours: &[f32], reference: &[f32]) -> f64 {
    let (mut signal, mut noise) = (0f64, 0f64);
    for (a, b) in ours.iter().zip(reference) {
        signal += (*b as f64).powi(2);
        noise += (*a as f64 - *b as f64).powi(2);
    }
    10.0 * (signal / noise.max(1e-30)).log10()
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // `Db` is used from Task 6
enum Exact {
    Bits,
    Db(f64),
}

fn assert_close(name: &str, what: &str, ours: &[f32], reference: &[f32], exact: Exact) {
    match exact {
        Exact::Bits => assert!(ours == reference, "{name}: {what} is not bit-exact"),
        Exact::Db(min) => {
            let db = snr_db(ours, reference);
            eprintln!("{name}: {what} {db:.1} dB");
            assert!(db >= min, "{name}: {what} {db:.1} dB < {min} dB");
        }
    }
}

/// The whole native decode equals ffmpeg's: presentation starts at 0, same number of samples, the
/// same samples at offset 0.
fn assert_matches_ffmpeg(name: &str, exact: Exact) {
    let (_, ours) = decode_all(name);
    assert_eq!(ours.first_pts, Duration::ZERO, "{name}: presentation starts at 0");
    let Some(reference) = ffmpeg(name) else {
        eprintln!("skipped the ffmpeg comparison: no ffmpeg");
        return;
    };
    assert_eq!(ours.samples.len(), reference.len(), "{name}: sample count ({} channels)", ours.channels);
    assert_close(name, "decode", &ours.samples, &reference, exact);
}

/// After seeking to `t` minus the stream's pre-roll (as the player does), the samples at `t` are
/// ffmpeg's samples at `t`.
fn assert_seek(name: &str, t: Duration, exact: Exact) {
    let Some(reference) = ffmpeg(name) else { return };
    let mut d = open(name);
    let s = audio(&*d);
    d.seek(t.saturating_sub(s.seek_preroll)).unwrap();
    let want = t + Duration::from_millis(150);
    let out = decode(&mut *d, &s, |o| o.rate > 0 && o.end() > want);
    assert!(out.first_pts <= t, "{name}: seeking to {t:?} landed after it, at {:?}", out.first_pts);
    let ch = out.channels as usize;
    let index = |time: Duration| (time.as_secs_f64() * out.rate as f64).round() as usize * ch;
    let ours = &out.samples[index(t - out.first_pts).min(out.samples.len())..];
    let theirs = &reference[index(t).min(reference.len())..];
    let n = (2048 * ch).min(ours.len()).min(theirs.len());
    assert!(n >= 64 * ch, "{name}: too little audio after seeking to {t:?}");
    assert_close(name, &format!("after seeking to {t:?}"), &ours[..n], &theirs[..n], exact);
}

/// Plays `name` to the end through `Player` (null audio output, ffmpeg disabled).
fn plays_to_the_end(name: &str) -> Player {
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig {
        audio_output: AudioOutputConfig::Null(null.clone()),
        ffmpeg: FfmpegConfig { enabled: false, ..Default::default() },
        ..Default::default()
    };
    let player = Player::open(Source::parse(if name.starts_with('/') { name.to_string() } else { fixture(name) }.as_str()).unwrap(), config).unwrap();
    assert!(!player.has_video() && player.has_audio(), "{name}: audio only");
    assert!(player.duration().is_some_and(|d| d > Duration::ZERO), "{name}: has a duration");
    player.play();
    let start = Instant::now();
    while !matches!(player.state(), PlayerState::Ended) {
        assert!(start.elapsed() < Duration::from_secs(20), "{name}: did not end ({:?})", player.state());
        null.pull(4800);
        std::thread::sleep(Duration::from_millis(2));
    }
    player
}

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

// ---- WAV ----

#[test]
fn wav_formats_match_ffmpeg_exactly() {
    for name in ["wav_s16.wav", "wav_s24.wav", "wav_f32.wav", "wav_u8.wav", "wav_51.wav"] {
        assert_matches_ffmpeg(name, Exact::Bits);
    }
}

#[test]
fn wav_seeks_exactly() {
    assert_seek("wav_s16.wav", ms(333), Exact::Bits);
    assert_seek("wav_51.wav", ms(250), Exact::Bits);
}

#[test]
fn wav_unsupported_formats_name_the_codec() {
    let d = open("wav_adpcm.wav");
    let err = match Registry::empty_with_native().open_audio_decoder(&audio(&*d), None) {
        Ok(_) => panic!("ADPCM is not supported"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("IMA ADPCM"), "{err}");
}

#[test]
fn wav_list_info_tags() {
    let player = plays_to_the_end("wav_tagged.wav");
    let m = player.metadata().expect("tags");
    assert_eq!((m.title.as_deref(), m.artist.as_deref(), m.album.as_deref()), (Some("Test Title"), Some("Test Artist"), Some("Test Album")));
    assert_eq!((m.track, m.year, m.genre.as_deref()), (Some(3), Some(2024), Some("Rock")));
}

#[test]
fn wav_plays_to_the_end() {
    let p = plays_to_the_end("wav_s16.wav");
    assert_eq!(p.metadata(), None, "no tags");
}

/// RIFF chunks of a WAV file: (fourcc, body).
fn riff_chunks(b: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 12;
    while pos + 8 <= b.len() {
        let size = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        out.push((b[pos..pos + 4].try_into().unwrap(), b[pos + 8..(pos + 8 + size).min(b.len())].to_vec()));
        pos += 8 + size + (size & 1);
    }
    out
}

#[test]
fn wav_with_odd_chunks_and_list_first_opens() {
    let original = std::fs::read(fixture("wav_s16.wav")).unwrap();
    let chunks = riff_chunks(&original);
    let get = |id: &[u8; 4]| chunks.iter().find(|(c, _)| c == id).unwrap().1.clone();
    let mut list = b"INFOINAM".to_vec();
    list.extend(4u32.to_le_bytes());
    list.extend(b"Odd\0");
    let mut body = b"WAVE".to_vec();
    for (id, data) in [(b"junk", b"abc".to_vec()), (b"LIST", list), (b"fmt ", get(b"fmt ")), (b"data", get(b"data"))] {
        body.extend(id);
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(&data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut file = b"RIFF".to_vec();
    file.extend((body.len() as u32).to_le_bytes());
    file.extend(body);
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("odd_chunks.wav");
    std::fs::write(&path, file).unwrap();
    let mut d = open_path(path.to_str().unwrap());
    assert_eq!(d.metadata().and_then(|m| m.title.clone()).as_deref(), Some("Odd"));
    let s = audio(&*d);
    let ours = decode(&mut *d, &s, |_| false);
    assert_eq!(ours.samples, decode_all("wav_s16.wav").1.samples);
}

