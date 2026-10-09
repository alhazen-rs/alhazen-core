//! HLS end to end against an in-process server: VOD (TS, fMP4), separate audio on another host,
//! AES-128, discontinuities, seeking, live, errors.
#![cfg(feature = "hls")]

#[path = "support/server.rs"]
mod server;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alhazen_core::audio::{AudioOutputConfig, NullOutput};
use alhazen_core::backend::Registry;
use alhazen_core::demux::{Demuxer, Packet, StreamInfo, StreamKind};
use alhazen_core::hls::HlsDemuxer;
use alhazen_core::{Error, Player, PlayerConfig, PlayerState, Source};
use server::Server;
use url::Url;

fn root() -> std::path::PathBuf {
    format!("{}/tests/fixtures/hls", env!("CARGO_MANIFEST_DIR")).into()
}

fn demux(url: &str) -> HlsDemuxer {
    HlsDemuxer::open(&Url::parse(url).unwrap(), &Registry::with_defaults()).unwrap()
}

fn all(d: &mut HlsDemuxer) -> Vec<(Packet, Option<StreamInfo>)> {
    let mut out = Vec::new();
    while let Some(p) = d.next_packet().unwrap() {
        let update = d.take_stream_update();
        out.push((p, update));
    }
    out
}

fn kind_of(streams: &[StreamInfo], id: u32) -> StreamKind {
    streams.iter().find(|s| s.id == id).unwrap().kind
}

/// First video and first audio pts, and the last pts of each.
fn span(streams: &[StreamInfo], packets: &[(Packet, Option<StreamInfo>)], kind: StreamKind) -> (Duration, Duration, usize) {
    let mut pts: Vec<Duration> = packets.iter().filter(|(p, _)| kind_of(streams, p.stream) == kind).map(|(p, _)| p.pts).collect();
    pts.sort();
    (pts[0], *pts.last().unwrap(), pts.len())
}

struct Played {
    frames: usize,
    sizes: BTreeSet<(u32, u32)>,
    state: PlayerState,
}

/// Plays to the end (or `limit`) with a fake sound card consuming audio in real time.
fn play(url: &str, limit: Duration) -> Played {
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig { audio_output: AudioOutputConfig::Null(null.clone()), decoder_threads: 2, ..Default::default() };
    let player = Player::open(Source::parse(url).unwrap(), config).unwrap();
    player.play();
    let start = Instant::now();
    let (mut pulled, mut frames, mut last, mut sizes) = (0usize, 0, None, BTreeSet::new());
    while start.elapsed() < limit && !matches!(player.state(), PlayerState::Ended | PlayerState::Error(_)) {
        if let Some(f) = player.current_frame()
            && last != Some(f.pts())
        {
            last = Some(f.pts());
            frames += 1;
            sizes.insert(f.size());
        }
        let due = (start.elapsed().as_secs_f64() * 48_000.0) as usize;
        null.pull(due - pulled);
        pulled = due;
        std::thread::sleep(Duration::from_millis(5));
    }
    Played { frames, sizes, state: player.state() }
}

#[test]
fn ts_vod_demuxes_in_sync_and_plays_to_the_end() {
    let server = Server::dir(root());
    let mut d = demux(&server.url("ts/index.m3u8"));
    assert!(!d.is_live());
    let streams = d.streams().to_vec();
    assert_eq!(streams.iter().map(|s| (s.id, s.kind)).collect::<Vec<_>>(), [(1, StreamKind::Video), (2, StreamKind::Audio)]);
    assert_eq!(streams[0].duration, Some(Duration::from_secs(6)));
    let packets = all(&mut d);
    let (v0, v1, vn) = span(&streams, &packets, StreamKind::Video);
    let (a0, a1, _) = span(&streams, &packets, StreamKind::Audio);
    assert!(v0 < Duration::from_millis(100) && a0 < Duration::from_millis(100), "both start at 0: {v0:?} {a0:?}");
    assert!(v0.abs_diff(a0) < Duration::from_millis(50), "in sync: {v0:?} {a0:?}");
    assert!(v1 > Duration::from_millis(5800) && a1 > Duration::from_millis(5800), "{v1:?} {a1:?}");
    assert_eq!(vn, 150);
    let played = play(&server.url("ts/index.m3u8"), Duration::from_secs(30));
    assert_eq!(played.state, PlayerState::Ended);
    assert!(played.frames >= 120, "{} frames", played.frames);
}

#[test]
fn fmp4_vod_plays_to_the_end() {
    let server = Server::dir(root());
    let mut d = demux(&server.url("fmp4/index.m3u8"));
    let streams = d.streams().to_vec();
    assert_eq!(streams[0].codec, alhazen_core::demux::Codec::Hevc);
    let packets = all(&mut d);
    let (v0, _, vn) = span(&streams, &packets, StreamKind::Video);
    let (a0, _, _) = span(&streams, &packets, StreamKind::Audio);
    assert!(v0.abs_diff(a0) < Duration::from_millis(50), "{v0:?} {a0:?}");
    assert_eq!(vn, 100);
    let played = play(&server.url("fmp4/index.m3u8"), Duration::from_secs(30));
    assert_eq!(played.state, PlayerState::Ended);
    assert!(played.frames >= 80, "{} frames", played.frames);
}

/// The master playlist with its audio rendition moved to `audio_server`.
fn master_with_remote_audio(video: &Server, audio: &Server) {
    let text = std::fs::read_to_string(root().join("multi/master.m3u8")).unwrap();
    video.set_body("multi/master.m3u8", text.replace("URI=\"audio/index.m3u8\"", &format!("URI=\"{}\"", audio.url("multi/audio/index.m3u8"))));
}

#[test]
fn separate_audio_on_another_host_is_in_sync() {
    let (video, audio) = (Server::dir(root()), Server::dir(root()));
    master_with_remote_audio(&video, &audio);
    let mut d = demux(&video.url("multi/master.m3u8"));
    let streams = d.streams().to_vec();
    assert_eq!(streams.iter().map(|s| s.kind).collect::<Vec<_>>(), [StreamKind::Video, StreamKind::Audio]);
    let packets = all(&mut d);
    let (v0, v1, _) = span(&streams, &packets, StreamKind::Video);
    let (a0, a1, an) = span(&streams, &packets, StreamKind::Audio);
    assert!(v0.abs_diff(a0) < Duration::from_millis(50), "in sync: {v0:?} {a0:?}");
    assert!(v1.abs_diff(a1) < Duration::from_millis(100), "{v1:?} {a1:?}");
    assert!(an > 250, "{an} AAC frames");
    assert!(audio.hits("multi/audio/seg0.ts") >= 1, "audio came from the other host");
    assert_eq!(video.hits("multi/audio/seg0.ts"), 0);
    // Packets come out interleaved by time, not one track after the other.
    let first_audio = packets.iter().position(|(p, _)| p.stream == 2).unwrap();
    assert!(first_audio < 20, "audio starts early in the stream: {first_audio}");
    let played = play(&video.url("multi/master.m3u8"), Duration::from_secs(30));
    assert_eq!(played.state, PlayerState::Ended);
}

#[test]
fn aes128_stream_plays() {
    let server = Server::dir(root());
    let mut d = demux(&server.url("aes/index.m3u8"));
    let streams = d.streams().to_vec();
    let (_, _, vn) = span(&streams, &all(&mut d), StreamKind::Video);
    assert_eq!(vn, 75);
    assert_eq!(server.hits("aes/key.bin"), 1, "the key is fetched once");
}

#[test]
fn discontinuity_continues_the_timeline_and_reconfigures() {
    let server = Server::dir(root());
    let mut d = demux(&server.url("disc/index.m3u8"));
    let streams = d.streams().to_vec();
    let packets = all(&mut d);
    let video: Vec<_> = packets.iter().filter(|(p, _)| p.stream == 1).collect();
    let update = video.iter().position(|(_, u)| u.is_some()).expect("a stream update at the discontinuity");
    let info = video[update].1.as_ref().unwrap();
    assert_eq!((info.width, info.height), (160, 90));
    assert!(video[update].0.pts.abs_diff(Duration::from_secs(3)) < Duration::from_millis(50), "{:?}", video[update].0.pts);
    assert!(video[update].0.keyframe);
    let (_, a1, _) = span(&streams, &packets, StreamKind::Audio);
    assert!(a1 > Duration::from_millis(5800), "audio continues after the discontinuity: {a1:?}");
    let played = play(&server.url("disc/index.m3u8"), Duration::from_secs(30));
    assert_eq!(played.state, PlayerState::Ended);
    assert!(played.sizes.contains(&(320, 180)) && played.sizes.contains(&(160, 90)), "{:?}", played.sizes);
}

#[test]
fn vod_seek_lands_on_the_keyframe_before_the_target() {
    let server = Server::dir(root());
    let mut d = demux(&server.url("ts/index.m3u8"));
    let at = d.seek(Duration::from_millis(3500)).unwrap();
    assert!(at.abs_diff(Duration::from_secs(3)) < Duration::from_millis(50), "{at:?}");
    let p = std::iter::from_fn(|| d.next_packet().unwrap()).find(|p| p.stream == 1).unwrap();
    assert!(p.keyframe);
    assert_eq!(p.pts, at);
    let at = d.seek(Duration::ZERO).unwrap();
    assert!(at < Duration::from_millis(50));
}

#[test]
fn live_plays_and_ends_when_the_playlist_stops() {
    let server = Server::dir(root());
    server.live("ts/live.m3u8", "ts/index.m3u8", root(), 3, Duration::from_secs(1), 3);
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig { audio_output: AudioOutputConfig::Null(null.clone()), decoder_threads: 2, ..Default::default() };
    let player = Player::open(Source::parse(&server.url("ts/live.m3u8")).unwrap(), config).unwrap();
    assert_eq!(player.duration(), None);
    drop(player);
    let played = play(&server.url("ts/live.m3u8"), Duration::from_secs(30));
    assert_eq!(played.state, PlayerState::Ended);
    assert!(played.frames >= 60, "{} frames", played.frames);
}

#[test]
fn drm_is_refused_at_open() {
    let server = Server::dir(root());
    server.set_body(
        "drm.m3u8",
        "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:1,\nts/seg0.ts\n#EXT-X-ENDLIST\n",
    );
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, ..Default::default() };
    match Player::open(Source::parse(&server.url("drm.m3u8")).unwrap(), config) {
        Err(Error::Unsupported(m)) => assert!(m.contains("DRM"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("opened"),
    }
}

#[test]
fn a_missing_segment_fails_clearly() {
    let server = Server::dir(root());
    server.fail("ts/seg2.ts", 404, usize::MAX);
    let played = play(&server.url("ts/index.m3u8"), Duration::from_secs(30));
    match played.state {
        PlayerState::Error(e) => assert!(e.to_string().contains("404") && e.to_string().contains("seg2.ts"), "{e}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn dropping_during_a_stalled_download_is_quick() {
    let server = Server::dir(root());
    server.stall("ts/seg2.ts", Duration::from_secs(30));
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, decoder_threads: 2, ..Default::default() };
    let player = Player::open(Source::parse(&server.url("ts/index.m3u8")).unwrap(), config).unwrap();
    player.play();
    std::thread::sleep(Duration::from_millis(2500));
    let t = Instant::now();
    drop(player);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn dash_is_still_unsupported() {
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, ..Default::default() };
    assert!(matches!(Player::open(Source::parse("https://example.com/a.mpd").unwrap(), config), Err(Error::Unsupported(_))));
}
