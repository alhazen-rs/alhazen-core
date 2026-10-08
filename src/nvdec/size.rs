//! The size NVDEC's hardware scaler should produce.

/// Output size for a `display`-sized picture shown in at most `max` (device pixels): fits within
/// `max` keeping the aspect ratio, never larger than `display`, even, at least 2×2. A missing or
/// zero `max` means no limit.
pub fn target_size(display: (u32, u32), max: Option<(u32, u32)>) -> (u32, u32) {
    let (w, h) = (display.0 as u64, display.1 as u64);
    let (mut tw, mut th) = (w, h);
    if let Some((mw, mh)) = max.filter(|&(mw, mh)| mw > 0 && mh > 0) {
        let (mw, mh) = (mw as u64, mh as u64);
        if w > mw || h > mh {
            // Scale by the tighter of the two limits.
            if w * mh > h * mw {
                (tw, th) = (mw, h * mw / w);
            } else {
                (tw, th) = (w * mh / h, mh);
            }
        }
    }
    let even = |v: u64| ((v & !1).max(2)) as u32;
    (even(tw), even(th))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_the_display_area_keeping_the_aspect_ratio() {
        assert_eq!(target_size((3840, 2160), Some((1280, 720))), (1280, 720));
        assert_eq!(target_size((3840, 2160), Some((1000, 1000))), (1000, 562));
        assert_eq!(target_size((1920, 800), Some((1280, 1280))), (1280, 532));
    }

    #[test]
    fn never_enlarges() {
        assert_eq!(target_size((1920, 1080), Some((4000, 4000))), (1920, 1080));
        assert_eq!(target_size((1920, 1080), None), (1920, 1080));
    }

    #[test]
    fn rounds_down_to_even_and_stays_at_least_2x2() {
        assert_eq!(target_size((1921, 1081), None), (1920, 1080));
        assert_eq!(target_size((4, 4), Some((1, 1))), (2, 2));
        assert_eq!(target_size((3, 3), None), (2, 2));
    }

    #[test]
    fn a_zero_limit_means_no_limit() {
        assert_eq!(target_size((640, 480), Some((0, 0))), (640, 480));
    }
}
