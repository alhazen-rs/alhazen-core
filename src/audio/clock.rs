//! The audio-driven master clock.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::output::OutputShared;
use crate::clock::{Clock, SystemClock};

struct Base {
    time: Duration,
    frames: u64,
    /// Running on wall time (audio failed or ran out).
    wall: bool,
}

/// Time = base + frames actually played since `set` / rate − device latency.
/// Freezes while paused and during underruns. When audio can no longer advance time — the device
/// failed, or the audio stream ended before the video — it continues on a wall clock from where
/// audio stopped. A seek returns it to audio time unless the device failed.
pub struct AudioClock {
    out: Arc<OutputShared>,
    base: Mutex<Base>,
    fallback: SystemClock,
}

impl AudioClock {
    pub(crate) fn new(out: Arc<OutputShared>) -> Self {
        Self {
            out,
            base: Mutex::new(Base { time: Duration::ZERO, frames: 0, wall: false }),
            fallback: SystemClock::new(),
        }
    }

    fn audio_now(&self, base: &Base) -> Duration {
        let played = self.out.frames_played.load(Ordering::Acquire).saturating_sub(base.frames);
        let played = Duration::from_nanos(played * 1_000_000_000 / self.out.rate.max(1) as u64);
        (base.time + played.saturating_sub(self.out.latency())).max(base.time)
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
        base.frames = self.out.frames_played.load(Ordering::Acquire);
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
    use crate::audio::output::push_frames;

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
    fn set_rebases_and_latency_is_subtracted() {
        let null = NullOutput::new(1000, 1);
        let mut out = null.attach(Arc::new(Volume::default()));
        let clock = AudioClock::new(out.shared.clone());
        clock.resume();
        push_frames(&mut out.producer, &out.shared, &[0.1; 200], || true);
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
