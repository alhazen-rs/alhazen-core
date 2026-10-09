//! Container detection by content (never by extension), including the audio-file formats.
#![cfg(feature = "native")]

use std::io::Read;

use alhazen_core::demux::{ContainerFormat, probe};
use alhazen_core::source::FileSource;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn probe_file(path: &str) -> Option<ContainerFormat> {
    probe(&mut FileSource::open(path).unwrap()).unwrap()
}

fn probe_bytes(name: &str, bytes: &[u8]) -> Option<ContainerFormat> {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, bytes).unwrap();
    probe_file(path.to_str().unwrap())
}

#[test]
fn every_audio_file_format_is_detected() {
    use ContainerFormat::*;
    for (name, want) in [
        ("mp3_cbr.mp3", Mp3),
        ("mp3_vbr.mp3", Mp3),
        ("mp3_mpeg2.mp3", Mp3),
        ("mp3_mpeg25.mp3", Mp3),
        ("mp3_no_xing.mp3", Mp3),
        ("mp3_tagged.mp3", Mp3),
        ("aac.aac", Adts),
        ("aac_tagged.aac", Adts),
        ("flac.flac", Flac),
        ("flac_tagged.flac", Flac),
        ("wav_s16.wav", Wav),
        ("wav_adpcm.wav", Wav),
        ("vorbis.ogg", Ogg),
        ("opus.opus", Ogg),
        ("flac.oga", Ogg),
    ] {
        assert_eq!(probe_file(&fixture(name)), Some(want), "{name}");
    }
}

#[test]
fn non_audio_and_lone_frame_headers_are_not_detected() {
    assert_eq!(probe_file(&fixture("not_video.bin")), None);
    // One valid MP3 header followed by zeros: no chain of frames.
    let mut lone = vec![0xFF, 0xFB, 0x90, 0x64];
    lone.resize(4096, 0);
    assert_eq!(probe_bytes("lone_header.bin", &lone), None);
    // Pseudo-random bytes salted with 0xFF sync bytes.
    let mut x = 0x1234_5678u32;
    let noise: Vec<u8> = (0..65_536)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            if i % 97 == 0 { 0xFF } else { x as u8 }
        })
        .collect();
    assert_eq!(probe_bytes("noise.bin", &noise), None);
}

#[test]
fn probe_skips_a_2mb_id3_tag_and_a_second_tag() {
    let mp3 = std::fs::read(fixture("mp3_cbr.mp3")).unwrap();
    assert!(mp3.starts_with(b"ID3"), "the fixture starts with its own ID3v2 tag");
    let size = 2_000_000u32;
    let mut big = b"ID3\x03\x00\x00".to_vec();
    big.extend([(size >> 21) as u8 & 0x7F, (size >> 14) as u8 & 0x7F, (size >> 7) as u8 & 0x7F, size as u8 & 0x7F]);
    big.resize(10 + size as usize, 0); // padding only
    big.extend(&mp3);
    assert_eq!(probe_bytes("big_id3.mp3", &big), Some(ContainerFormat::Mp3));
}

#[test]
fn probe_leaves_the_source_at_the_start() {
    let mut src = FileSource::open(fixture("mp3_tagged.mp3")).unwrap();
    probe(&mut src).unwrap();
    let mut head = [0u8; 3];
    src.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"ID3");
}
