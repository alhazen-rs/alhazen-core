//! Adaptive bitrate: which variant to fetch next, from measured throughput and buffer level.

use std::collections::VecDeque;
use std::time::Duration;

/// Only this share of the estimated throughput is spent on the stream.
const SAFETY: f64 = 0.7;
/// Switching up needs this much headroom over the next variant's bitrate...
const UP_HEADROOM: f64 = 1.4;
/// ...for this many consecutive downloads.
const UP_SAMPLES: usize = 3;
/// A first download smaller than this is too short to time.
const MIN_PROBE_BYTES: u64 = 16 * 1024;

/// Throughput estimate (the lower of a fast and a slow moving average, so drops count at once
/// and rises only once they last) and the switching rules built on it.
pub(crate) struct Abr {
    bitrates: Vec<u64>,
    fast: Option<f64>,
    slow: Option<f64>,
    /// The latest estimates, for the "sustained surplus" rule.
    recent: VecDeque<f64>,
}

impl Abr {
    pub fn new(bitrates: Vec<u64>) -> Self {
        Self { bitrates, fast: None, slow: None, recent: VecDeque::new() }
    }

    /// Bits per second, if anything was measured.
    pub fn estimate(&self) -> Option<f64> {
        Some(self.fast?.min(self.slow?))
    }

    /// One download of `bytes` that took `elapsed`.
    pub fn sample(&mut self, bytes: u64, elapsed: Duration) {
        let bps = bytes as f64 * 8.0 / elapsed.as_secs_f64().max(1e-3);
        let mix = |avg: Option<f64>, half_life: f64| {
            let alpha = 1.0 - 0.5f64.powf(1.0 / half_life);
            Some(avg.map_or(bps, |a| a + alpha * (bps - a)))
        };
        self.fast = mix(self.fast, 2.0);
        self.slow = mix(self.slow, 5.0);
        self.push_recent();
    }

    /// A download failed: treated as a collapse of throughput.
    pub fn fail(&mut self) {
        let collapsed = self.estimate().unwrap_or(0.0) / 4.0;
        self.fast = Some(collapsed);
        self.slow = Some(collapsed);
        self.push_recent();
    }

    fn push_recent(&mut self) {
        if let Some(e) = self.estimate() {
            self.recent.push_back(e);
            if self.recent.len() > UP_SAMPLES {
                self.recent.pop_front();
            }
        }
    }

    /// The variant to fetch next while playing `current` with `buffer` of media ahead.
    pub fn choose(&mut self, current: usize, buffer: Duration, segment: Duration, blacklist: &[bool]) -> usize {
        let Some(estimate) = self.estimate() else { return current };
        let allowed = |i: usize| !blacklist.get(i).copied().unwrap_or(false);
        let rate = |i: usize| self.bitrates[i];
        if !allowed(current) || buffer < segment || estimate < rate(current) as f64 {
            let pick = best_within(&self.bitrates, SAFETY * estimate, blacklist).unwrap_or(current);
            if pick != current {
                self.recent.clear();
            }
            return pick;
        }
        // One step up: the next higher bitrate that may be used.
        let next = (0..self.bitrates.len()).filter(|&i| allowed(i) && rate(i) > rate(current)).min_by_key(|&i| rate(i));
        if let Some(next) = next
            && buffer >= segment * 2
            && self.recent.len() >= UP_SAMPLES
            && self.recent.iter().all(|&e| e >= UP_HEADROOM * rate(next) as f64)
        {
            self.recent.clear();
            return next;
        }
        current
    }

    /// The first variant: from the time the playlist download took, else the middle one.
    pub fn initial(bitrates: &[u64], probe_bytes: u64, probe_elapsed: Duration, blacklist: &[bool]) -> usize {
        let allowed: Vec<usize> = (0..bitrates.len()).filter(|&i| !blacklist.get(i).copied().unwrap_or(false)).collect();
        if probe_bytes >= MIN_PROBE_BYTES {
            let bps = probe_bytes as f64 * 8.0 / probe_elapsed.as_secs_f64().max(1e-3);
            if let Some(i) = best_within(bitrates, SAFETY * bps, blacklist) {
                return i;
            }
        }
        let mut by_rate = allowed;
        by_rate.sort_by_key(|&i| bitrates[i]);
        by_rate.get(by_rate.len() / 2).copied().unwrap_or(0)
    }
}

/// The highest allowed bitrate not above `budget`; the lowest allowed one when none fits.
fn best_within(bitrates: &[u64], budget: f64, blacklist: &[bool]) -> Option<usize> {
    let allowed = |i: &usize| !blacklist.get(*i).copied().unwrap_or(false);
    let fitting = (0..bitrates.len()).filter(allowed).filter(|&i| bitrates[i] as f64 <= budget).max_by_key(|&i| bitrates[i]);
    fitting.or_else(|| (0..bitrates.len()).filter(allowed).min_by_key(|&i| bitrates[i]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [u64; 3] = [500_000, 1_500_000, 4_000_000];

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    /// `n` downloads at `bps` bits per second.
    fn feed(abr: &mut Abr, bps: u64, n: usize) {
        for _ in 0..n {
            abr.sample(bps / 8, secs(1.0));
        }
    }

    #[test]
    fn collapse_switches_down_at_once() {
        let mut abr = Abr::new(RATES.to_vec());
        feed(&mut abr, 10_000_000, 6);
        assert_eq!(abr.choose(2, secs(20.0), secs(4.0), &[false; 3]), 2);
        feed(&mut abr, 800_000, 2);
        assert_eq!(abr.choose(2, secs(20.0), secs(4.0), &[false; 3]), 2, "estimate (5.4 Mb/s) still above 4 Mb/s");
        feed(&mut abr, 800_000, 2);
        assert_eq!(abr.choose(2, secs(20.0), secs(4.0), &[false; 3]), 1, "under it (3.1 Mb/s): down at once, as far as 0.7 × estimate says");
        feed(&mut abr, 800_000, 6);
        assert_eq!(abr.choose(1, secs(20.0), secs(4.0), &[false; 3]), 0, "0.7 × estimate fits only the lowest");
    }

    #[test]
    fn up_one_step_only_after_sustained_surplus_and_buffer() {
        let mut abr = Abr::new(RATES.to_vec());
        feed(&mut abr, 10_000_000, 2);
        assert_eq!(abr.choose(0, secs(20.0), secs(4.0), &[false; 3]), 0, "2 samples are not enough");
        feed(&mut abr, 10_000_000, 1);
        assert_eq!(abr.choose(0, secs(4.0), secs(4.0), &[false; 3]), 0, "buffer under 2 segments");
        assert_eq!(abr.choose(0, secs(20.0), secs(4.0), &[false; 3]), 1, "one step at a time");
    }

    #[test]
    fn low_buffer_forces_down_and_blacklist_is_respected() {
        let mut abr = Abr::new(RATES.to_vec());
        feed(&mut abr, 3_000_000, 6);
        assert_eq!(abr.choose(2, secs(1.0), secs(4.0), &[false; 3]), 1);
        assert_eq!(abr.choose(2, secs(1.0), secs(4.0), &[false, true, false]), 0);
        assert_eq!(abr.choose(1, secs(20.0), secs(4.0), &[false, true, false]), 0, "a blacklisted current variant is left");
    }

    #[test]
    fn steady_bandwidth_does_not_oscillate() {
        let mut abr = Abr::new(RATES.to_vec());
        let (mut current, mut switches) = (0, 0);
        for _ in 0..40 {
            abr.sample(2_400_000 / 8, secs(1.0));
            let next = abr.choose(current, secs(12.0), secs(4.0), &[false; 3]);
            if next != current {
                switches += 1;
                current = next;
            }
        }
        assert_eq!(current, 1);
        assert!(switches <= 1, "{switches} switches");
    }

    #[test]
    fn failures_count_as_a_collapse() {
        let mut abr = Abr::new(RATES.to_vec());
        feed(&mut abr, 10_000_000, 6);
        abr.fail();
        abr.fail();
        assert_eq!(abr.choose(2, secs(20.0), secs(4.0), &[false; 3]), 0);
    }

    #[test]
    fn initial_choice() {
        assert_eq!(Abr::initial(&RATES, 128 * 1024, secs(0.1), &[false; 3]), 2, "about 10.5 Mb/s");
        assert_eq!(Abr::initial(&RATES, 64 * 1024, secs(0.1), &[false; 3]), 1, "about 5.2 Mb/s");
        assert_eq!(Abr::initial(&RATES, 1000, secs(0.1), &[false; 3]), 1, "too small to measure: the middle");
        assert_eq!(Abr::initial(&RATES, 128 * 1024, secs(0.1), &[false, false, true]), 1);
    }
}
