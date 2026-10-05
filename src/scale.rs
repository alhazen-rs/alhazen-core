//! Downscaling decoded pictures to the size they are displayed at, before colour conversion:
//! a 4K frame shown in a 1080p window then costs a quarter of the conversion and of the upload.

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::decode::{PixelLayout, YuvFrame, chroma_size};

/// Frames are only scaled when this would shrink them below this share of their size; close
/// to 1:1 the cost is not worth the softening.
const MIN_SHRINK: f64 = 0.9;

/// The size `width`×`height` scales to so it fits in `max` (aspect kept, even dimensions), or
/// `None` if it already (nearly) fits.
pub fn fit_within(width: u32, height: u32, max: (u32, u32)) -> Option<(u32, u32)> {
    let (mw, mh) = max;
    if width == 0 || height == 0 || mw == 0 || mh == 0 {
        return None;
    }
    let scale = (mw as f64 / width as f64).min(mh as f64 / height as f64);
    if scale >= MIN_SHRINK {
        return None;
    }
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    Some((even(width as f64 * scale), even(height as f64 * scale)))
}

/// Reuses the resizer's buffers across frames.
#[derive(Default)]
pub struct FrameScaler {
    resizer: Resizer,
}

impl FrameScaler {
    /// `frame` scaled to fit in `max`, or `None` when it already fits. Runs on the current
    /// rayon pool.
    pub fn downscale(&mut self, frame: &YuvFrame, max: (u32, u32)) -> Option<YuvFrame> {
        let (tw, th) = fit_within(frame.width, frame.height, max)?;
        let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
        let (cw, ch) = frame.chroma_size();
        let (tcw, tch) = chroma_size(frame.layout, tw, th);
        let dims = [(frame.width, frame.height, tw, th), (cw, ch, tcw, tch), (cw, ch, tcw, tch)];
        let mut planes: [Vec<u8>; 3] = Default::default();
        for (i, &(w, h, nw, nh)) in dims.iter().enumerate() {
            if frame.layout == PixelLayout::I400 && i > 0 {
                continue;
            }
            let packed = pack(&frame.planes[i], frame.strides[i], w as usize, h as usize);
            let src = ImageRef::new(w, h, &packed, PixelType::U8).ok()?;
            let mut dst = Image::new(nw, nh, PixelType::U8);
            self.resizer.resize(&src, &mut dst, &options).ok()?;
            planes[i] = dst.into_vec();
        }
        Some(YuvFrame {
            width: tw,
            height: th,
            layout: frame.layout,
            planes,
            strides: [tw as usize, tcw as usize, tcw as usize],
            matrix: frame.matrix,
            full_range: frame.full_range,
            pts: frame.pts,
        })
    }
}

/// `plane` without row padding (borrowed when there is none).
fn pack(plane: &[u8], stride: usize, w: usize, h: usize) -> std::borrow::Cow<'_, [u8]> {
    if stride == w {
        return std::borrow::Cow::Borrowed(&plane[..w * h]);
    }
    std::borrow::Cow::Owned((0..h).flat_map(|y| &plane[y * stride..y * stride + w]).copied().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::ColorMatrix;
    use std::time::Duration;

    #[test]
    fn fits_keeping_aspect_with_even_sizes() {
        assert_eq!(fit_within(3840, 2160, (1920, 1200)), Some((1920, 1080)));
        assert_eq!(fit_within(3840, 2160, (1000, 1000)), Some((1000, 562)));
        assert_eq!(fit_within(1920, 1080, (1900, 1080)), None, "within 10 %: left alone");
        assert_eq!(fit_within(1920, 1080, (3840, 2160)), None, "never upscaled");
        assert_eq!(fit_within(1920, 1080, (0, 0)), None, "no limit");
    }

    #[test]
    fn downscales_all_planes_and_keeps_content() {
        // 64x48 4:2:0 with a horizontal luma ramp and flat chroma; Y rows padded to 70.
        let (w, h) = (64u32, 48u32);
        let y: Vec<u8> = (0..h).flat_map(|_| (0..70u32).map(|x| (x * 4).min(255) as u8)).collect();
        let frame = YuvFrame {
            width: w,
            height: h,
            layout: PixelLayout::I420,
            planes: [y, vec![90; 32 * 24], vec![200; 32 * 24]],
            strides: [70, 32, 32],
            matrix: ColorMatrix::Bt709,
            full_range: false,
            pts: Duration::from_millis(7),
        };
        let small = FrameScaler::default().downscale(&frame, (32, 32)).unwrap();
        assert_eq!((small.width, small.height, small.layout, small.pts), (32, 24, PixelLayout::I420, frame.pts));
        assert_eq!((small.planes[0].len(), small.planes[1].len(), small.planes[2].len()), (32 * 24, 16 * 12, 16 * 12));
        assert!(small.planes[1].iter().all(|&v| v == 90) && small.planes[2].iter().all(|&v| v == 200));
        // The ramp survives: left dark, right bright, increasing along the row.
        let row = &small.planes[0][..32];
        assert!(row[0] < 20 && row[31] > 230, "{row:?}");
        assert!(row.windows(2).all(|p| p[0] <= p[1]), "{row:?}");
    }

    #[test]
    fn fitting_frames_are_left_alone() {
        let frame = YuvFrame {
            width: 4,
            height: 4,
            layout: PixelLayout::I420,
            planes: [vec![0; 16], vec![0; 4], vec![0; 4]],
            strides: [4, 2, 2],
            matrix: ColorMatrix::Bt709,
            full_range: false,
            pts: Duration::ZERO,
        };
        assert!(FrameScaler::default().downscale(&frame, (8, 8)).is_none());
    }
}
