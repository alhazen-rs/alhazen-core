//! Audio playback end to end, headless: a `NullOutput` stands in for the sound card and the
//! test "plays" audio by pulling samples from it.
#![cfg(feature = "native")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use video_core::audio::{AudioOutputConfig, NullOutput};
use video_core::{Player, PlayerConfig, PlayerEvent, PlayerState, Source};

const RATE: u32 = 48_000;

fn fixture(name: &str) -> Source {
    Source::parse(&format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn open(name: &str) -> (Player, Arc<NullOutput>) {
    let null = NullOutput::new(RATE, 2);
    let config = PlayerConfig {
        decoder_threads: 2,
        audio_output: AudioOutputConfig::Null(null.clone()),
        ..Default::default()
    };
    (Player::open(fixture(name), config).unwrap(), null)
}

/// Plays `ms` milliseconds of audio at real-time pace, like a device: 10 ms callbacks, 10 ms apart.
fn play_ms(null: &NullOutput, ms: u64) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..ms.div_ceil(10) {
        out.extend(null.pull((RATE / 100) as usize));
        std::thread::sleep(Duration::from_millis(10));
    }
    out
}

fn peak(s: &[f32]) -> f32 {
    s.iter().fold(0.0, |m, x| m.max(x.abs()))
}

/// Calls `step` until it returns `Some`, failing after `secs` seconds.
fn until<T>(what: &str, secs: u64, mut step: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = step() {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn video_follows_the_audio_clock() {
    let (player, null) = open("av1_with_audio.webm");
    assert!(player.has_video() && player.has_audio());
    player.play();
    let mut checked = 0;
    until("end of playback", 20, || {
        play_ms(&null, 10);
        if let Some(f) = player.current_frame() {
            let pos = player.position();
            if pos > Duration::from_millis(100) && pos < Duration::from_millis(1900) {
                // The frame on screen is the one due at the audio position (30 fps: 33 ms apart).
                let diff = pos.abs_diff(f.pts());
                assert!(diff <= Duration::from_millis(45), "A/V offset {diff:?} at {pos:?}");
                checked += 1;
            }
        }
        (player.state() == PlayerState::Ended).then_some(())
    });
    assert!(checked > 100, "only {checked} sync checks");
}

#[test]
fn surround_opus_is_downmixed_to_stereo() {
    // 5.1 with a tone on the centre only: in stereo the centre goes equally to left and right.
    let (player, null) = open("opus_51_center.webm");
    assert!(player.has_audio());
    player.play();
    let s = until("sound", 5, || {
        let s = play_ms(&null, 50);
        (peak(&s) > 0.02).then_some(s)
    });
    let (mut left, mut right) = (0.0f32, 0.0f32);
    for f in s.chunks(2) {
        left += f[0] * f[0];
        right += f[1] * f[1];
    }
    assert!((left - right).abs() < left.max(right) * 0.05, "centre must be balanced: L {left} R {right}");
}

#[test]
fn audio_only_file_plays_sound_to_the_end() {
    let (player, null) = open("opus_only.webm");
    assert!(!player.has_video() && player.has_audio());
    assert!(player.current_frame().is_none());
    player.play();
    let mut loud = 0;
    until("end", 20, || {
        let s = play_ms(&null, 20);
        if peak(&s) > 0.05 {
            loud += 1;
        }
        (player.state() == PlayerState::Ended).then_some(())
    });
    // ~2 s of tone in 20 ms pulls.
    assert!((90..=105).contains(&loud), "{loud} loud chunks");
}

#[test]
fn pause_freezes_the_clock_and_silences_output() {
    let (player, null) = open("opus_only.webm");
    player.play();
    until("audio playing", 5, || {
        play_ms(&null, 20);
        (player.position() > Duration::from_millis(300)).then_some(())
    });
    player.pause();
    let at_pause = player.position();
    let s = play_ms(&null, 200);
    assert_eq!(peak(&s), 0.0);
    assert_eq!(player.position(), at_pause);
    player.play();
    play_ms(&null, 100);
    assert!(player.position() > at_pause);
}

#[test]
fn seek_restarts_audio_at_the_target() {
    let (player, null) = open("opus_only.webm");
    player.play();
    until("audio playing", 5, || {
        play_ms(&null, 20);
        (player.position() > Duration::from_millis(200)).then_some(())
    });
    player.seek(Duration::from_millis(1500));
    assert_eq!(player.position(), Duration::from_millis(1500));
    // After the seek, the clock resumes from 1.5 s, never from the old position.
    until("audio after seek", 5, || {
        play_ms(&null, 10);
        let p = player.position();
        assert!(p >= Duration::from_millis(1500), "clock went back to {p:?}");
        (p > Duration::from_millis(1600)).then_some(())
    });
}

#[test]
fn volume_and_mute_scale_the_output() {
    let (player, null) = open("opus_only.webm");
    player.play();
    let full = until("sound", 5, || {
        let s = play_ms(&null, 50);
        (peak(&s) > 0.05).then(|| peak(&s))
    });
    player.set_volume(0.5);
    let half = peak(&play_ms(&null, 50));
    assert!((half - full / 2.0).abs() < full * 0.15, "full {full}, half {half}");
    player.set_muted(true);
    assert_eq!(peak(&play_ms(&null, 50)), 0.0);
    assert!(player.is_muted());
    assert_eq!(player.volume(), 0.5);
}

#[test]
fn device_loss_falls_back_to_wall_clock_and_keeps_video() {
    let (player, null) = open("av1_with_audio.webm");
    let events = player.events();
    player.play();
    until("playing", 5, || {
        play_ms(&null, 10);
        player.current_frame(); // a UI consumes frames every repaint
        (player.position() > Duration::from_millis(200)).then_some(())
    });
    null.simulate_device_loss();
    let at_loss = player.position();
    // Nobody pulls audio any more, yet time and video keep going on the wall clock.
    let f = until("a video frame after the loss", 5, || {
        std::thread::sleep(Duration::from_millis(10));
        player.current_frame().filter(|f| f.pts() > at_loss + Duration::from_millis(200))
    });
    assert!(f.pts() > at_loss);
    assert!(player.position() >= at_loss + Duration::from_millis(200));
    let _ = events.try_iter().count();
    assert!(!player.state().is_error());
}

#[cfg(feature = "native-aac")]
#[test]
fn aac_audio_only_m4a_plays() {
    let (player, null) = open("aac_only.m4a");
    assert!(player.has_audio());
    player.play();
    until("sound", 5, || (peak(&play_ms(&null, 50)) > 0.05).then_some(()));
}

#[cfg(not(feature = "native-aac"))]
#[test]
fn aac_without_native_aac_plays_video_silently_with_warning() {
    let (player, _null) = open("av1_aac.mp4");
    assert!(player.has_video() && !player.has_audio());
    assert!(player.events().try_iter().any(|e| matches!(e, PlayerEvent::Warning(_))));
}

#[cfg(not(feature = "native-aac"))]
#[test]
fn aac_audio_only_without_native_aac_is_an_error() {
    let config = PlayerConfig { audio_output: AudioOutputConfig::Null(NullOutput::new(RATE, 2)), ..Default::default() };
    assert!(Player::open(fixture("aac_only.m4a"), config).is_err());
}

#[test]
fn video_keeps_playing_after_a_shorter_audio_track_ends() {
    // 2 s of video, 1 s of audio: once audio runs out, time must keep going for the video.
    let (player, null) = open("av1_short_audio.webm");
    player.play();
    let mut last = Duration::ZERO;
    until("end of playback", 15, || {
        play_ms(&null, 10);
        if let Some(f) = player.current_frame() {
            last = last.max(f.pts());
        }
        (player.state() == PlayerState::Ended).then_some(())
    });
    assert!(last >= Duration::from_millis(1900), "video stopped at {last:?}");
}

#[test]
fn device_loss_warns_disables_audio_and_still_ends() {
    let (player, null) = open("av1_with_audio.webm");
    let events = player.events();
    player.play();
    until("playing", 5, || {
        play_ms(&null, 10);
        player.current_frame();
        (player.position() > Duration::from_millis(200)).then_some(())
    });
    null.simulate_device_loss();
    until("end after device loss", 15, || {
        std::thread::sleep(Duration::from_millis(10));
        player.current_frame();
        (player.state() == PlayerState::Ended).then_some(())
    });
    assert!(!player.has_audio(), "audio must be reported as gone");
    assert!(
        events.try_iter().any(|e| matches!(&e, PlayerEvent::Warning(w) if w.contains("audio"))),
        "a Warning must say audio was lost"
    );
}

#[test]
fn laced_vorbis_without_default_duration_plays_completely() {
    // Frames inside a laced block share one timestamp; they must play back to back, not be
    // dropped as overlaps.
    let (player, null) = open("laced_vorbis.webm");
    player.play();
    let mut loud = 0;
    until("end", 20, || {
        if peak(&play_ms(&null, 20)) > 0.05 {
            loud += 1;
        }
        (player.state() == PlayerState::Ended).then_some(())
    });
    assert!((90..=105).contains(&loud), "{loud} loud 20 ms chunks for 2 s of tone");
}
