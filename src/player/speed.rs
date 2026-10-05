//! Detects video decoding that cannot keep up, to switch to a faster backend (once).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Decode cost is averaged over this much wall time.
const WINDOW: Duration = Duration::from_secs(1);
/// Too slow for this long, continuously, before switching.
const SUSTAIN: Duration = Duration::from_millis(1500);
/// Too slow means the average cost exceeds this share of the frame interval.
const BUDGET: f64 = 0.95;
/// Fewer samples than this in the window say nothing yet.
const MIN_SAMPLES: usize = 8;
/// Late-frame drops in this many consecutive windows of `DROP_WINDOW` also mean too slow.
const DROP_WINDOWS: u32 = 3;
const DROP_WINDOW: Duration = Duration::from_millis(500);
/// Frames reaching the queue this late (behind the playback clock), continuously for `SUSTAIN`,
/// also mean too slow — whatever the cause (a threaded decoder's cost hides inside its own
/// threads, so per-call cost alone can look fine while the picture falls behind the sound).
pub(super) const LATE_LIMIT: Duration = Duration::from_millis(100);

pub(super) struct SpeedMonitor {
    /// (when, decode+convert cost, pts) per decoded frame, oldest first.
    samples: VecDeque<(Instant, Duration, Duration)>,
    slow_since: Option<Instant>,
    drop_window: Option<(Instant, u64)>,
    drop_streak: u32,
    /// The decoder was behind at every sample of the current drop window.
    behind_in_window: bool,
    /// Since when every frame has reached the queue more than `LATE_LIMIT` late.
    late_since: Option<Instant>,
}

impl SpeedMonitor {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            slow_since: None,
            drop_window: None,
            drop_streak: 0,
            behind_in_window: true,
            late_since: None,
        }
    }

    /// Forgets everything: after a seek, a pause, or while buffering.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn record_frame(&mut self, now: Instant, cost: Duration, pts: Duration) {
        self.samples.push_back((now, cost, pts));
        while self.samples.front().is_some_and(|(t, ..)| now.duration_since(*t) > WINDOW) {
            self.samples.pop_front();
        }
    }

    /// `dropped` is the renderer's running count of frames discarded as late. `decoder_behind`
    /// says the frame queue was (nearly) empty: only then are drops the decoder's fault. A fast
    /// decoder keeps the queue full, and drops then just mean the display refreshes slower than
    /// the video's frame rate (120 fps on a 60 Hz screen) — no reason to switch.
    pub fn record_drops(&mut self, now: Instant, dropped: u64, decoder_behind: bool) {
        let (start, at_start) = *self.drop_window.get_or_insert((now, dropped));
        if !decoder_behind {
            self.behind_in_window = false;
        }
        if now.duration_since(start) >= DROP_WINDOW {
            let counts = dropped > at_start && self.behind_in_window;
            self.drop_streak = if counts { self.drop_streak + 1 } else { 0 };
            self.drop_window = Some((now, dropped));
            self.behind_in_window = true;
        }
    }

    /// How late (behind the playback clock) the latest frame reached the queue.
    pub fn record_lateness(&mut self, now: Instant, late: Duration) {
        if late > LATE_LIMIT {
            self.late_since.get_or_insert(now);
        } else {
            self.late_since = None;
        }
    }

    /// Average decode+convert cost and frame interval over the window, once there is enough data.
    fn averages(&self) -> Option<(Duration, Duration)> {
        if self.samples.len() < MIN_SAMPLES {
            return None;
        }
        let n = self.samples.len() as u32;
        let cost = self.samples.iter().map(|(_, c, _)| *c).sum::<Duration>() / n;
        let (lo, hi) = self.samples.iter().fold((Duration::MAX, Duration::ZERO), |(lo, hi), (_, _, p)| (lo.min(*p), hi.max(*p)));
        let interval = hi.checked_sub(lo)? / (n - 1);
        (interval > Duration::ZERO).then_some((cost, interval))
    }

    /// Whether decoding has been too slow for long enough to switch.
    pub fn too_slow(&mut self, now: Instant) -> bool {
        if self.drop_streak >= DROP_WINDOWS {
            return true;
        }
        if self.late_since.is_some_and(|since| now.duration_since(since) >= SUSTAIN) {
            return true;
        }
        match self.averages() {
            Some((cost, interval)) if cost.as_secs_f64() > interval.as_secs_f64() * BUDGET => {
                now.duration_since(*self.slow_since.get_or_insert(now)) >= SUSTAIN
            }
            Some(_) => {
                self.slow_since = None;
                false
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds 30 fps frames costing `cost` each, 33 ms apart in wall time, for `secs` seconds.
    fn run(m: &mut SpeedMonitor, start: Instant, cost_ms: u64, secs: f64) -> Option<Duration> {
        let frames = (secs * 30.0) as u64;
        for i in 0..frames {
            let now = start + Duration::from_millis(i * 33);
            m.record_frame(now, Duration::from_millis(cost_ms), Duration::from_millis(i * 33));
            if m.too_slow(now) {
                return Some(now - start);
            }
        }
        None
    }

    #[test]
    fn fast_decoding_never_switches() {
        assert_eq!(run(&mut SpeedMonitor::new(), Instant::now(), 20, 10.0), None);
    }

    #[test]
    fn sustained_slow_decoding_switches_after_the_sustain_time() {
        let at = run(&mut SpeedMonitor::new(), Instant::now(), 40, 10.0).expect("switch");
        // Needs MIN_SAMPLES first, then SUSTAIN of continuous slowness.
        assert!(at >= SUSTAIN && at < SUSTAIN + Duration::from_millis(400), "switched at {at:?}");
    }

    #[test]
    fn a_short_slow_burst_does_not_switch() {
        let mut m = SpeedMonitor::new();
        let start = Instant::now();
        assert_eq!(run(&mut m, start, 40, 1.0), None);
        assert_eq!(run(&mut m, start + Duration::from_secs(1), 10, 5.0), None);
    }

    #[test]
    fn reset_restarts_the_sustain_timer() {
        let mut m = SpeedMonitor::new();
        let start = Instant::now();
        assert_eq!(run(&mut m, start, 40, 1.2), None);
        m.reset();
        let at = run(&mut m, start + Duration::from_millis(1200), 40, 10.0).expect("switch");
        assert!(at >= SUSTAIN);
    }

    #[test]
    fn drops_in_three_consecutive_windows_switch() {
        let mut m = SpeedMonitor::new();
        let t = Instant::now();
        let mut dropped = 0;
        for w in 0..=3u64 {
            assert!(!m.too_slow(t + DROP_WINDOW * w as u32) || w == 3);
            dropped += 2;
            m.record_drops(t + DROP_WINDOW * w as u32, dropped, true);
        }
        assert!(m.too_slow(t + DROP_WINDOW * 3));
    }

    #[test]
    fn drops_with_a_full_queue_are_the_display_not_the_decoder() {
        // 120 fps on a 60 Hz screen: every other frame is skipped, but the decoder keeps up.
        let mut m = SpeedMonitor::new();
        let t = Instant::now();
        for w in 0..10u32 {
            m.record_drops(t + DROP_WINDOW * w, 30 * w as u64, false);
        }
        assert!(!m.too_slow(t + DROP_WINDOW * 10));
    }

    #[test]
    fn frames_arriving_late_for_the_sustain_time_switch() {
        // Cheap decoder calls (threaded decoder) but every frame reaches the queue 300 ms late.
        let mut m = SpeedMonitor::new();
        let t = Instant::now();
        let mut at = None;
        for i in 0..120u64 {
            let now = t + Duration::from_millis(i * 16);
            m.record_frame(now, Duration::from_millis(1), Duration::from_millis(i * 16));
            m.record_lateness(now, Duration::from_millis(300));
            if m.too_slow(now) {
                at = Some(now - t);
                break;
            }
        }
        let at = at.expect("switch");
        assert!(at >= SUSTAIN && at < SUSTAIN + Duration::from_millis(100), "switched at {at:?}");
    }

    #[test]
    fn a_short_late_spell_does_not_switch() {
        let mut m = SpeedMonitor::new();
        let t = Instant::now();
        for i in 0..400u64 {
            let now = t + Duration::from_millis(i * 16);
            // Late for 1 s (e.g. after a hiccup), then on time again.
            let late = if i < 60 { 300 } else { 5 };
            m.record_frame(now, Duration::from_millis(1), Duration::from_millis(i * 16));
            m.record_lateness(now, Duration::from_millis(late));
            assert!(!m.too_slow(now), "switched at frame {i}");
        }
    }

    #[test]
    fn a_window_without_drops_resets_the_streak() {
        let mut m = SpeedMonitor::new();
        let t = Instant::now();
        for (w, dropped) in [(0u32, 0u64), (1, 2), (2, 4), (3, 4), (4, 6), (5, 8)].into_iter() {
            m.record_drops(t + DROP_WINDOW * w, dropped, true);
        }
        assert!(!m.too_slow(t + DROP_WINDOW * 5));
    }
}
