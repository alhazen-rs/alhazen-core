//! Presentation clocks.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
    fn pause(&self);
    fn resume(&self);
    fn set(&self, t: Duration);
    fn is_paused(&self) -> bool;
    /// Playback speed multiplier; clocks that cannot scale ignore it.
    fn set_speed(&self, _speed: f64) {}
}

struct State {
    base: Duration,
    started: Option<Instant>,
    speed: f64,
}

/// Wall-clock based clock. Starts paused at zero.
pub struct SystemClock {
    state: Mutex<State>,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { state: Mutex::new(State { base: Duration::ZERO, started: None, speed: 1.0 }) }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        let s = self.state.lock().unwrap();
        s.base + s.started.map(|t| t.elapsed().mul_f64(s.speed)).unwrap_or_default()
    }
    fn pause(&self) {
        let mut s = self.state.lock().unwrap();
        if let Some(t) = s.started.take() {
            let speed = s.speed;
            s.base += t.elapsed().mul_f64(speed);
        }
    }
    fn resume(&self) {
        let mut s = self.state.lock().unwrap();
        if s.started.is_none() {
            s.started = Some(Instant::now());
        }
    }
    fn set(&self, t: Duration) {
        let mut s = self.state.lock().unwrap();
        s.base = t;
        if s.started.is_some() {
            s.started = Some(Instant::now());
        }
    }
    fn is_paused(&self) -> bool {
        self.state.lock().unwrap().started.is_none()
    }
    fn set_speed(&self, speed: f64) {
        let mut s = self.state.lock().unwrap();
        if let Some(t) = s.started.take() {
            let old = s.speed;
            s.base += t.elapsed().mul_f64(old);
            s.started = Some(Instant::now());
        }
        s.speed = speed;
    }
}

/// Manually driven clock for tests: time only moves via `advance`/`set`.
pub struct MockClock {
    state: Mutex<(Duration, bool)>,
}

impl MockClock {
    pub fn new() -> Self {
        Self { state: Mutex::new((Duration::ZERO, true)) }
    }
    /// Advances time if the clock is running.
    pub fn advance(&self, d: Duration) {
        let mut s = self.state.lock().unwrap();
        if !s.1 {
            s.0 += d;
        }
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now(&self) -> Duration {
        self.state.lock().unwrap().0
    }
    fn pause(&self) {
        self.state.lock().unwrap().1 = true;
    }
    fn resume(&self) {
        self.state.lock().unwrap().1 = false;
    }
    fn set(&self, t: Duration) {
        self.state.lock().unwrap().0 = t;
    }
    fn is_paused(&self) -> bool {
        self.state.lock().unwrap().1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_starts_paused_and_runs_after_resume() {
        let c = SystemClock::new();
        assert!(c.is_paused());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.now(), Duration::ZERO);
        c.resume();
        std::thread::sleep(Duration::from_millis(30));
        assert!(c.now() >= Duration::from_millis(25));
        c.pause();
        let frozen = c.now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.now(), frozen);
    }

    #[test]
    fn system_clock_set_while_running() {
        let c = SystemClock::new();
        c.resume();
        c.set(Duration::from_secs(10));
        let t = c.now();
        assert!(t >= Duration::from_secs(10) && t < Duration::from_millis(10_050));
    }

    #[test]
    fn mock_clock_only_moves_when_running() {
        let c = MockClock::new();
        c.advance(Duration::from_secs(1));
        assert_eq!(c.now(), Duration::ZERO);
        c.resume();
        c.advance(Duration::from_secs(1));
        assert_eq!(c.now(), Duration::from_secs(1));
    }
}
