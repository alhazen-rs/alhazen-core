//! Maps each segment's raw timestamps onto the playlist timeline, keeping separately fetched audio
//! and video in sync.

use std::collections::HashMap;
use std::time::Duration;

/// Which playlist a segment came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    /// The variant's playlist (video, and its muxed audio).
    Main,
    /// A separate audio rendition.
    Audio,
}

impl Role {
    fn other(self) -> Role {
        match self {
            Role::Main => Role::Audio,
            Role::Audio => Role::Main,
        }
    }
}

/// One offset (timeline − raw, in nanoseconds) per playlist and discontinuity sequence, fixed by
/// the first segment of that sequence seen, so later segments follow their own exact timestamps
/// rather than the rounded `EXTINF` sums.
pub(crate) struct Timeline {
    offsets: HashMap<(Role, u64), i128>,
    /// Aligned renditions: the other playlist's offset is reused when it maps this segment within
    /// this much of where the playlist says it starts.
    tolerance: Duration,
}

impl Timeline {
    pub fn new(tolerance: Duration) -> Self {
        Self { offsets: HashMap::new(), tolerance }
    }

    /// The offset for a segment of `role` in discontinuity sequence `disc`, starting at `start`
    /// on the timeline, whose earliest timestamp is `first_raw`.
    pub fn anchor(&mut self, role: Role, disc: u64, start: Duration, first_raw: Duration) -> i128 {
        if let Some(&o) = self.offsets.get(&(role, disc)) {
            return o;
        }
        let own = start.as_nanos() as i128 - first_raw.as_nanos() as i128;
        let offset = match self.offsets.get(&(role.other(), disc)) {
            Some(&o) if (own - o).unsigned_abs() <= self.tolerance.as_nanos() => o,
            _ => own,
        };
        self.offsets.insert((role, disc), offset);
        offset
    }

    /// Like `anchor`, but always from this segment's own timing (never the other playlist's):
    /// after a jump to the live edge, the old offsets would put a hole in playback time.
    pub fn reanchor(&mut self, role: Role, disc: u64, start: Duration, first_raw: Duration) -> i128 {
        self.forget(role);
        let own = start.as_nanos() as i128 - first_raw.as_nanos() as i128;
        self.offsets.insert((role, disc), own);
        own
    }

    /// Drops `role`'s offsets (another rendition takes its place).
    pub fn forget(&mut self, role: Role) {
        self.offsets.retain(|(r, _), _| *r != role);
    }

    /// `raw` on the timeline (never before 0).
    pub fn map(offset: i128, raw: Duration) -> Duration {
        let t = raw.as_nanos() as i128 + offset;
        Duration::from_nanos(t.clamp(0, u64::MAX as i128) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(secs: f64) -> Duration {
        Duration::from_secs_f64(secs)
    }

    #[test]
    fn a_sequence_keeps_its_first_offset() {
        let mut t = Timeline::new(s(1.0));
        let o = t.anchor(Role::Main, 0, s(0.0), s(10.0));
        assert_eq!(Timeline::map(o, s(10.5)), s(0.5));
        // A later segment whose EXTINF sum (4.0) is off from its real start (4.02): its own
        // timestamps win.
        let o2 = t.anchor(Role::Main, 0, s(4.0), s(14.02));
        assert_eq!(o2, o);
        assert_eq!(Timeline::map(o2, s(14.02)), s(4.02));
    }

    #[test]
    fn a_discontinuity_reanchors() {
        let mut t = Timeline::new(s(1.0));
        t.anchor(Role::Main, 0, s(0.0), s(10.0));
        let o = t.anchor(Role::Main, 1, s(3.0), s(1.4));
        assert_eq!(Timeline::map(o, s(1.4)), s(3.0));
    }

    #[test]
    fn aligned_audio_reuses_the_video_offset() {
        let mut t = Timeline::new(s(1.0));
        let v = t.anchor(Role::Main, 0, s(0.0), s(10.0));
        // Audio's segment starts at 0 too, but its first sample is 21 ms before the video's.
        let a = t.anchor(Role::Audio, 0, s(0.0), s(9.979));
        assert_eq!(a, v, "same clock: sample-exact sync");
        assert_eq!(Timeline::map(a, s(9.979)), Duration::ZERO, "clamped at 0");
    }

    #[test]
    fn unrelated_clocks_use_their_own_offset() {
        let mut t = Timeline::new(s(1.0));
        t.anchor(Role::Main, 0, s(0.0), s(10.0));
        let a = t.anchor(Role::Audio, 0, s(0.0), s(500.0));
        assert_eq!(Timeline::map(a, s(500.0)), Duration::ZERO);
    }
}
