//! The audio-driven master clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::output::OutputShared;
use crate::clock::{Clock, SystemClock};

struct Base {
    time: Duration,
    frames: u64,
}

/// Time = base + frames actually played since `set` / rate − device latency.
/// Freezes while paused and during underruns. If the device fails, it continues on a wall clock
/// from where audio stopped.
pub struct AudioClock {
    out: Arc<OutputShared>,
    base: Mutex<Base>,
    fallback: SystemClock,
    fell_back: AtomicBool,
}

impl AudioClock {
    pub(crate) fn new(out: Arc<OutputShared>) -> Self {
        Self {
            out,
            base: Mutex::new(Base { time: Duration::ZERO, frames: 0 }),
            fallback: SystemClock::new(),
            fell_back: AtomicBool::new(false),
        }
    }

    fn audio_now(&self) -> Duration {
        let base = self.base.lock().unwrap();
        let played = self.out.frames_played.load(Ordering::Acquire).saturating_sub(base.frames);
        let played = Duration::from_nanos(played * 1_000_000_000 / self.out.rate.max(1) as u64);
        (base.time + played.saturating_sub(self.out.latency())).max(base.time)
    }

    /// `true` once the device failed and the clock runs on wall time.
    pub fn is_fallback(&self) -> bool {
        self.fell_back.load(Ordering::Relaxed)
    }
}

impl Clock for AudioClock {
    fn now(&self) -> Duration {
        if self.out.failed.load(Ordering::Relaxed) && !self.fell_back.swap(true, Ordering::AcqRel) {
            self.fallback.set(self.audio_now());
            if !self.out.paused.load(Ordering::Relaxed) {
                self.fallback.resume();
            }
        }
        if self.is_fallback() { self.fallback.now() } else { self.audio_now() }
    }
    fn pause(&self) {
        self.out.paused.store(true, Ordering::Relaxed);
        self.fallback.pause();
    }
    fn resume(&self) {
        self.out.paused.store(false, Ordering::Relaxed);
        if self.is_fallback() {
            self.fallback.resume();
        }
    }
    fn set(&self, t: Duration) {
        let mut base = self.base.lock().unwrap();
        base.time = t;
        base.frames = self.out.frames_played.load(Ordering::Acquire);
        self.fallback.set(t);
    }
    fn is_paused(&self) -> bool {
        self.out.paused.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{NullOutput, Volume};
    use crate::audio::output::push_frames;

    #[test]
    fn advances_only_with_played_frames_and_freezes_when_paused() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        assert!(clock.is_paused());
        push_frames(&mut out.producer, 1, &[0.1; 150], || true);
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
    fn set_rebases_and_latency_is_subtracted() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        push_frames(&mut out.producer, 1, &[0.1; 200], || true);
        null.pull(100);
        clock.set(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(5));
        out.shared.latency_ns.store(20_000_000, Ordering::Relaxed);
        null.pull(50);
        assert_eq!(clock.now(), Duration::from_millis(5_030), "50 ms played − 20 ms latency");
    }

    #[test]
    fn device_failure_falls_back_to_wall_clock_from_current_position() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        push_frames(&mut out.producer, 1, &[0.1; 100], || true);
        null.pull(100);
        out.shared.failed.store(true, Ordering::Relaxed);
        let at_failure = clock.now();
        assert!(clock.is_fallback());
        assert!(at_failure >= Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(30));
        assert!(clock.now() >= at_failure + Duration::from_millis(25), "keeps running on wall time");
    }
}
