//! Display order for VideoToolbox's pictures, which come out in decode order: a window of
//! `depth` pictures is held back and released in pts order.

use std::time::Duration;

/// The window never holds back more than this many pictures (H.264/HEVC allow at most 16).
pub(crate) const MAX_DEPTH: usize = 16;

/// Whether a picture at `pts` can still go into the window, given the last picture released.
/// A picture that comes too late (before the last one released) is dropped, and the window grows
/// by one from then on, up to `MAX_DEPTH`. Repeated timestamps are not late.
pub(crate) fn admit(last_out: Option<Duration>, pts: Duration, depth: &mut usize) -> bool {
    if last_out.is_some_and(|last| pts < last) {
        *depth = (*depth + 1).min(MAX_DEPTH);
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn a_picture_after_the_last_released_is_admitted() {
        let mut depth = 2;
        assert!(admit(None, MS(0), &mut depth));
        assert!(admit(Some(MS(40)), MS(80), &mut depth));
        assert_eq!(depth, 2);
    }

    #[test]
    fn a_late_picture_is_dropped_and_the_window_grows() {
        let mut depth = 2;
        assert!(!admit(Some(MS(80)), MS(40), &mut depth));
        assert_eq!(depth, 3);
    }

    #[test]
    fn a_repeated_timestamp_is_not_late() {
        // Some files give several pictures the same pts (missing or rounded timestamps): they
        // are not out of order, so nothing is dropped and the window does not grow.
        let mut depth = 2;
        assert!(admit(Some(MS(40)), MS(40), &mut depth));
        assert_eq!(depth, 2);
    }

    #[test]
    fn the_window_never_grows_past_sixteen() {
        let mut depth = 15;
        for _ in 0..10 {
            admit(Some(MS(80)), MS(40), &mut depth);
        }
        assert_eq!(depth, MAX_DEPTH);
    }
}
