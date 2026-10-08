//! Headless end-to-end tests of the player pipeline, driven by a `MockClock`.
#![cfg(feature = "native")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::clock::MockClock;
use alhazen_core::{Error, Player, PlayerConfig, PlayerEvent, PlayerState, Source};

fn fixture(name: &str) -> Source {
    Source::parse(&format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn open(name: &str) -> (Player, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new());
    let config = PlayerConfig {
        clock: Some(clock.clone()),
        decoder_threads: 2,
        // Never touch the real sound device from tests.
        audio_output: AudioOutputConfig::Disabled,
        ..Default::default()
    };
    (Player::open(fixture(name), config).unwrap(), clock)
}

/// Polls `f` until it returns `Some`, failing after 5 seconds.
fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn opens_paused_with_metadata_and_first_frame() {
    let (player, _clock) = open("av1.webm");
    assert_eq!(player.state(), PlayerState::Paused);
    assert_eq!(player.duration(), Some(Duration::from_secs(2)));
    assert_eq!(player.video_size(), Some((320, 240)));
    let frame = wait_for("first frame", || player.current_frame());
    assert_eq!(frame.pts(), Duration::ZERO);
    assert_eq!(frame.size(), (320, 240));
}

#[test]
fn plays_through_in_order_and_ends() {
    for name in ["av1.webm", "av1.mp4"] {
        let (player, clock) = open(name);
        let events = player.events();
        player.play();
        let mut seen = Vec::new();
        let start = Instant::now();
        // Real-time pace: a faster clock than the decoder can follow makes the player skip late
        // frames on purpose (catch-up), which is not what this test is about. The clock starts
        // with the first frame, as a viewer sees it: on slow machines (CI's macOS runners) the
        // decoder's start-up would otherwise make the first frames late and skipped.
        let mut last = Instant::now();
        while player.state() != PlayerState::Ended {
            assert!(start.elapsed() < Duration::from_secs(10), "{name}: never ended");
            if let Some(f) = player.current_frame()
                && seen.last() != Some(&f.pts())
            {
                seen.push(f.pts());
            }
            let now = Instant::now();
            if !seen.is_empty() {
                clock.advance(now - last);
            }
            last = now;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{name}: frames out of order");
        assert!(seen.len() >= 55, "{name}: only {} distinct frames shown", seen.len());
        assert!(events.try_iter().any(|e| matches!(e, PlayerEvent::Ended)));
    }
}

#[test]
fn seek_shows_frame_at_target_without_stale_frames() {
    let (player, _clock) = open("av1.webm");
    wait_for("first frame", || player.current_frame());
    player.seek(Duration::from_millis(1500));
    assert_eq!(player.position(), Duration::from_millis(1500));
    let frame = wait_for("frame after seek", || {
        player.current_frame().filter(|f| f.pts() > Duration::from_secs(1))
    });
    // The frame on screen at 1.5s is the one whose display interval contains 1.5s.
    assert!(frame.pts() <= Duration::from_millis(1500));
    assert!(frame.pts() > Duration::from_millis(1450));
}

#[test]
fn play_after_end_restarts() {
    let (player, clock) = open("av1.webm");
    player.play();
    player.seek(Duration::from_millis(1950));
    wait_for("ended", || {
        player.current_frame();
        clock.advance(Duration::from_millis(20));
        (player.state() == PlayerState::Ended).then_some(())
    });
    player.play();
    let f = wait_for("restart", || player.current_frame().filter(|f| f.pts() < Duration::from_millis(100)));
    assert_eq!(f.pts(), Duration::ZERO);
}

#[test]
fn disabled_audio_plays_video_only() {
    let (player, _clock) = open("av1_with_audio.webm");
    assert!(player.has_video());
    assert!(!player.has_audio());
    wait_for("first frame", || player.current_frame());
}

#[test]
fn truncated_file_ends_in_error_state() {
    let (player, clock) = open("truncated.webm");
    player.play();
    wait_for("error state", || {
        player.current_frame();
        clock.advance(Duration::from_millis(20));
        player.state().is_error().then_some(())
    });
}

#[test]
fn non_video_file_is_rejected() {
    let config = PlayerConfig { audio_output: AudioOutputConfig::Disabled, ..Default::default() };
    let err = Player::open(fixture("not_video.bin"), config).err().unwrap();
    assert!(matches!(err, Error::UnsupportedContainer));
}

#[test]
fn drop_during_playback_joins_threads() {
    let (player, clock) = open("av1.webm");
    player.play();
    clock.advance(Duration::from_millis(100));
    // Queue is full and the decode thread is blocked in push: drop must still return promptly.
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    drop(player);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn player_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Player>();
}

/// With a display size set, frames come out scaled down to fit it (aspect kept); the size can
/// change while playing (window resized).
#[test]
fn frames_are_scaled_to_the_output_size() {
    let clock = Arc::new(MockClock::new());
    let config = PlayerConfig {
        clock: Some(clock.clone()),
        decoder_threads: 2,
        audio_output: AudioOutputConfig::Disabled,
        max_output_size: Some((160, 160)),
        ..Default::default()
    };
    let player = Player::open(fixture("av1.webm"), config).unwrap();
    let frame = wait_for("first frame", || player.current_frame());
    assert_eq!(frame.size(), (160, 120));
    assert_eq!(player.video_size(), Some((320, 240)), "the video's own size is unchanged");

    player.set_max_output_size(None);
    player.play();
    wait_for("a full-size frame", || {
        clock.advance(Duration::from_millis(10));
        player.current_frame().filter(|f| f.size() == (320, 240))
    });
}
