//! The audio-driven master clock.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::output::{OutputShared, now_ns};
use crate::clock::{Clock, SystemClock};

struct Base {
    time: Duration,
    /// Seek epoch started by the last `set`; device reports from older epochs are ignored.
    epoch: u64,
    /// Running on wall time (audio failed or ran out).
    wall: bool,
}

/// Time = base + frames heard since `set` / rate, interpolated through the device buffer that is
/// playing (from the callback's anchor), so it moves smoothly rather than in buffer-sized steps.
/// Freezes while paused and during underruns; reports from callbacks that started before a
/// seek are ignored. When audio can no longer advance time — the device
/// failed, or the audio stream ended before the video — it continues on a wall clock from where
/// audio stopped. A seek returns it to audio time unless the device failed.
pub struct AudioClock {
    out: Arc<OutputShared>,
    base: Mutex<Base>,
    fallback: SystemClock,
}

impl AudioClock {
    pub(crate) fn new(out: Arc<OutputShared>) -> Self {
        let epoch = out.epoch();
        Self {
            out,
            base: Mutex::new(Base { time: Duration::ZERO, epoch, wall: false }),
            fallback: SystemClock::new(),
        }
    }

    fn audio_now(&self, base: &Base) -> Duration {
        let a = self.out.anchor();
        if a.epoch != base.epoch {
            return base.time;
        }
        let rate = self.out.rate.max(1) as u64;
        let heard = if a.len == 0 {
            a.frames_before
        } else {
            let into = now_ns().saturating_sub(a.at_ns);
            a.frames_before + (into as u128 * rate as u128 / 1_000_000_000).min(a.len as u128) as u64
        };
        base.time + Duration::from_nanos(heard * 1_000_000_000 / rate)
    }

    fn audio_gone(&self) -> bool {
        self.out.failed.load(Ordering::Relaxed) || self.out.exhausted.load(Ordering::Relaxed)
    }

    /// `true` while the clock runs on wall time.
    pub fn is_fallback(&self) -> bool {
        let base = self.base.lock().unwrap();
        base.wall || self.audio_gone()
    }
}

impl Clock for AudioClock {
    fn now(&self) -> Duration {
        let mut base = self.base.lock().unwrap();
        if !base.wall && self.audio_gone() {
            self.fallback.set(self.audio_now(&base));
            if !self.out.paused.load(Ordering::Relaxed) {
                self.fallback.resume();
            }
            base.wall = true;
        }
        if base.wall { self.fallback.now() } else { self.audio_now(&base) }
    }
    fn pause(&self) {
        let _base = self.base.lock().unwrap();
        self.out.paused.store(true, Ordering::Relaxed);
        self.fallback.pause();
    }
    fn resume(&self) {
        let base = self.base.lock().unwrap();
        self.out.paused.store(false, Ordering::Relaxed);
        if base.wall {
            self.fallback.resume();
        }
    }
    fn set(&self, t: Duration) {
        let mut base = self.base.lock().unwrap();
        base.time = t;
        base.epoch = self.out.next_epoch();
        self.fallback.set(t);
        base.wall = self.audio_gone();
    }
    fn is_paused(&self) -> bool {
        self.out.paused.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{NullOutput, Volume};
    use crate::audio::output::{Anchor, push_frames};

    #[test]
    fn advances_only_with_played_frames_and_freezes_when_paused() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        assert!(clock.is_paused());
        push_frames(&mut out.producer, &out.shared, &[0.1; 150], || true);
        null.pull(50);
        assert_eq!(clock.now(), Duration::ZERO, "paused: nothing played");
        clock.resume();
        null.pull(100);
        assert_eq!(clock.now(), Duration::from_millis(100));
        null.pull(100); // only 50 frames left: underrun must not advance time past them
        assert_eq!(clock.now(), Duration::from_millis(150));
        clock.pause();
        null.pull(100);
        assert_eq!(clock.now(), Duration::from_millis(150));
    }

    #[test]
    fn set_rebases_the_clock() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        push_frames(&mut out.producer, &out.shared, &[0.1; 200], || true);
        null.pull(100);
        clock.set(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(5));
        null.pull(50);
        assert_eq!(clock.now(), Duration::from_millis(5_050));
    }

    #[test]
    fn interpolates_within_a_device_buffer_and_honours_latency() {
        // A real device plays a callback's buffer over time, starting `delay` after the callback.
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        push_frames(&mut out.producer, &out.shared, &[0.1; 200], || true); // the ring holds 200 ms here
        // 100 ms buffer whose first frame reaches the speaker 30 ms from now.
        let rendered = std::time::Instant::now();
        null.render_realtime(100, Duration::from_millis(30));
        assert!(clock.now() < Duration::from_millis(5), "nothing audible yet: {:?}", clock.now());
        std::thread::sleep(Duration::from_millis(80));
        // Expected: time since the callback minus the 30 ms delay, within the 100 ms buffer.
        // Measured rather than assumed, so a sleep that overruns on a busy machine still checks.
        let (before, mid, after) = (rendered.elapsed(), clock.now(), rendered.elapsed());
        let expect = |t: Duration| t.saturating_sub(Duration::from_millis(30)).min(Duration::from_millis(100));
        let slack = Duration::from_millis(10);
        assert!(
            mid + slack >= expect(before) && mid <= expect(after) + slack,
            "mid-buffer: {mid:?}, expected {:?}..{:?}",
            expect(before),
            expect(after)
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(clock.now(), Duration::from_millis(100), "clamped at the end of what was played");
    }

    #[test]
    fn a_callback_that_started_before_a_seek_does_not_move_the_clock() {
        let null = NullOutput::new(1000, 1);
        let out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        let before_seek = out.shared.epoch();
        clock.set(Duration::from_secs(5));
        // That callback finishes now and reports 100 ms of (pre-seek) audio.
        out.shared.publish(Anchor { epoch: before_seek, frames_before: 100, len: 0, at_ns: 0 });
        assert_eq!(clock.now(), Duration::from_secs(5));
    }

    #[test]
    fn device_failure_falls_back_to_wall_clock_from_current_position() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        push_frames(&mut out.producer, &out.shared, &[0.1; 100], || true);
        null.pull(100);
        out.shared.failed.store(true, Ordering::Relaxed);
        let at_failure = clock.now();
        assert!(clock.is_fallback());
        assert!(at_failure >= Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(30));
        assert!(clock.now() >= at_failure + Duration::from_millis(25), "keeps running on wall time");
    }
}
