//! A demuxer whose stream changes format mid-way (as an HLS variant switch does): the player
//! reopens the decoder and keeps playing.
#![cfg(feature = "native")]

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::backend::Registry;
use alhazen_core::demux::{self, Demuxer, Packet, StreamInfo, StreamKind};
use alhazen_core::source::{FileSource, MediaSource};
use alhazen_core::{Player, PlayerConfig, PlayerState, Source};

/// Plays the video of each part in turn as one stream with id 1, announcing each new part's
/// format through `take_stream_update`.
struct Concat {
    parts: Vec<Box<dyn Demuxer>>,
    streams: Vec<StreamInfo>,
    offset: Duration,
    last: Duration,
    update: Option<StreamInfo>,
}

fn video(d: &dyn Demuxer) -> StreamInfo {
    let mut s = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().clone();
    s.id = 1;
    s
}

impl Demuxer for Concat {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> alhazen_core::Result<Option<Packet>> {
        loop {
            let Some(d) = self.parts.first_mut() else { return Ok(None) };
            let id = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().id;
            match d.next_packet()? {
                Some(mut p) if p.stream == id => {
                    p.stream = 1;
                    p.pts += self.offset;
                    self.last = self.last.max(p.pts);
                    return Ok(Some(p));
                }
                Some(_) => continue,
                None => {
                    self.parts.remove(0);
                    self.offset = self.last + Duration::from_millis(40);
                    if let Some(next) = self.parts.first() {
                        self.update = Some(video(next.as_ref()));
                    }
                }
            }
        }
    }

    fn seek(&mut self, _: Duration) -> alhazen_core::Result<Duration> {
        Ok(Duration::ZERO)
    }

    fn take_stream_update(&mut self) -> Option<StreamInfo> {
        self.update.take()
    }
}

fn open(name: &str) -> Box<dyn Demuxer> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let mut src: Box<dyn MediaSource> = Box::new(FileSource::open(&path).unwrap());
    let format = demux::probe(src.as_mut()).unwrap().unwrap();
    Registry::with_defaults().open_demuxer(&Source::File(path.into()), format, src, None).unwrap()
}

#[test]
fn a_stream_update_reopens_the_decoder_and_both_sizes_are_shown() {
    // AV1 then VP9: only a new decoder can play the second part.
    let (a, b) = (open("av1.webm"), open("vp9_tiles4.webm"));
    let (sa, sb) = (video(a.as_ref()), video(b.as_ref()));
    assert_ne!((sa.width, sa.height), (sb.width, sb.height), "the fixtures must differ in size");
    assert_ne!(sa.codec, sb.codec);
    let demuxer = Concat { streams: vec![sa.clone()], parts: vec![a, b], offset: Duration::ZERO, last: Duration::ZERO, update: None };
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, decoder_threads: 2, ..Default::default() };
    let player = Player::open_with_demuxer(Box::new(demuxer), false, config).unwrap();
    player.play();
    let mut sizes = BTreeSet::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) && player.state() != PlayerState::Ended {
        if let Some(f) = player.current_frame() {
            sizes.insert(f.size());
        }
        assert!(!matches!(player.state(), PlayerState::Error(_)), "{:?}", player.state());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(player.state(), PlayerState::Ended, "played to the end");
    assert!(sizes.contains(&(sa.width, sa.height)) && sizes.contains(&(sb.width, sb.height)), "sizes seen: {sizes:?}");
}
