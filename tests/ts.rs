//! MPEG-TS files (the format of most HLS segments).
#![cfg(feature = "native")]

use std::time::{Duration, Instant};

use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::backend::Registry;
use alhazen_core::demux::{self, Codec, ContainerFormat, Demuxer, StreamKind};
use alhazen_core::source::{FileSource, MediaSource};
use alhazen_core::{Player, PlayerConfig, PlayerState, Source};

fn path(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn open(name: &str) -> Box<dyn Demuxer> {
    let mut src: Box<dyn MediaSource> = Box::new(FileSource::open(path(name)).unwrap());
    let format = demux::probe(src.as_mut()).unwrap().unwrap();
    assert_eq!(format, ContainerFormat::MpegTs);
    Registry::empty_with_native().open_demuxer(&Source::File(path(name).into()), format, src, None).unwrap()
}

#[test]
fn ts_file_has_h264_and_aac_with_duration() {
    let d = open("h264_aac.ts");
    let video = d.streams().iter().find(|s| s.kind == StreamKind::Video).expect("video");
    assert_eq!((video.codec.clone(), video.width, video.height), (Codec::H264, 320, 180));
    assert_eq!(video.extradata.as_ref().unwrap()[0], 1, "avcC");
    let audio = d.streams().iter().find(|s| s.kind == StreamKind::Audio).expect("audio");
    assert_eq!((audio.codec.clone(), audio.sample_rate, audio.channels), (Codec::Aac, 48_000, 1));
    assert_eq!(audio.extradata.as_deref(), Some(&[0x11, 0x88][..]), "AAC-LC 48 kHz mono");
    let duration = video.duration.expect("duration");
    assert!(duration.abs_diff(Duration::from_secs(3)) < Duration::from_millis(100), "{duration:?}");
}

#[test]
fn ts_packets_start_at_zero_with_a_keyframe_and_are_length_prefixed() {
    let mut d = open("h264_aac.ts");
    let video = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().id;
    let mut first_video = None;
    let mut audio = 0;
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream == video {
            first_video.get_or_insert(p.clone());
            // 4-byte NAL lengths that cover the whole packet exactly.
            let mut pos = 0;
            while pos < p.data.len() {
                let len = u32::from_be_bytes(p.data[pos..pos + 4].try_into().unwrap()) as usize;
                pos += 4 + len;
            }
            assert_eq!(pos, p.data.len(), "NAL lengths tile the packet");
        } else {
            audio += 1;
        }
    }
    let first = first_video.unwrap();
    assert!(first.keyframe);
    assert!(first.pts < Duration::from_millis(100), "{:?}", first.pts);
    assert!(audio > 100, "{audio} AAC frames (about 141 in 3 s)");
}

#[test]
fn ts_file_seeks_to_a_keyframe() {
    let mut d = open("h264_aac.ts");
    let video = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().id;
    let at = d.seek(Duration::from_millis(2500)).unwrap();
    assert!(at.abs_diff(Duration::from_secs(2)) < Duration::from_millis(50), "keyframe before 2.5 s (1 s GOPs): {at:?}");
    let p = std::iter::from_fn(|| d.next_packet().unwrap()).find(|p| p.stream == video).unwrap();
    assert!(p.keyframe);
    assert_eq!(p.pts, at);
    let at = d.seek(Duration::ZERO).unwrap();
    assert!(at < Duration::from_millis(50), "{at:?}");
}

#[test]
fn ts_file_plays_to_the_end() {
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, decoder_threads: 2, ..Default::default() };
    let player = Player::open(Source::parse(&path("h264_aac.ts")).unwrap(), config).unwrap();
    assert_eq!(player.video_size(), Some((320, 180)));
    player.play();
    let (start, mut frames, mut last) = (Instant::now(), 0, None);
    while start.elapsed() < Duration::from_secs(30) && player.state() != PlayerState::Ended {
        assert!(!matches!(player.state(), PlayerState::Error(_)), "{:?}", player.state());
        if let Some(f) = player.current_frame()
            && last != Some(f.pts())
        {
            last = Some(f.pts());
            frames += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(player.state(), PlayerState::Ended);
    assert!(frames >= 60, "{frames} distinct frames of 75");
}
