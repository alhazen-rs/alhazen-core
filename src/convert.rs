//! YUV -> BGRA conversion (SIMD via the `yuv` crate, rows split across a rayon pool).

use yuv::{YuvGrayImage, YuvPlanarImage, YuvRange, YuvStandardMatrix};

use crate::decode::{ColorMatrix, PixelLayout, YuvFrame};
use crate::{Error, Result};

/// Converts `frame` into tightly packed BGRA (`width * 4` bytes per row).
/// Runs on the current rayon pool; call inside `pool.install` to choose the pool.
pub fn yuv_to_bgra(frame: &YuvFrame, out: &mut Vec<u8>) -> Result<()> {
    let (w, h) = (frame.width, frame.height);
    out.resize(w as usize * h as usize * 4, 0);
    let range = if frame.full_range { YuvRange::Full } else { YuvRange::Limited };
    let matrix = match frame.matrix {
        ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
        ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
        ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
    };
    let stride = w * 4;
    let result = if frame.layout == PixelLayout::I400 {
        let gray = YuvGrayImage {
            y_plane: &frame.planes[0],
            y_stride: frame.strides[0] as u32,
            width: w,
            height: h,
        };
        yuv::yuv400_to_bgra(&gray, out, stride, range, matrix)
    } else {
        let image = YuvPlanarImage {
            y_plane: &frame.planes[0],
            y_stride: frame.strides[0] as u32,
            u_plane: &frame.planes[1],
            u_stride: frame.strides[1] as u32,
            v_plane: &frame.planes[2],
            v_stride: frame.strides[2] as u32,
            width: w,
            height: h,
        };
        match frame.layout {
            PixelLayout::I420 => yuv::yuv420_to_bgra(&image, out, stride, range, matrix),
            PixelLayout::I422 => yuv::yuv422_to_bgra(&image, out, stride, range, matrix),
            _ => yuv::yuv444_to_bgra(&image, out, stride, range, matrix),
        }
    };
    result.map_err(|e| Error::Decode(format!("YUV->BGRA: {e:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn solid(y: u8, u: u8, v: u8, full_range: bool, matrix: ColorMatrix) -> YuvFrame {
        let (w, h) = (4u32, 4u32);
        YuvFrame {
            width: w,
            height: h,
            layout: PixelLayout::I420,
            planes: [vec![y; 16], vec![u; 4], vec![v; 4]],
            strides: [4, 2, 2],
            matrix,
            full_range,
            pts: Duration::ZERO,
        }
    }

    fn first_pixel(f: &YuvFrame) -> [u8; 4] {
        let mut out = Vec::new();
        yuv_to_bgra(f, &mut out).unwrap();
        assert_eq!(out.len(), 4 * 4 * 4);
        [out[0], out[1], out[2], out[3]]
    }

    fn close(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
    }

    #[test]
    fn limited_range_black_and_white() {
        assert!(close(first_pixel(&solid(16, 128, 128, false, ColorMatrix::Bt709)), [0, 0, 0, 255]));
        assert!(close(first_pixel(&solid(235, 128, 128, false, ColorMatrix::Bt709)), [255, 255, 255, 255]));
    }

    #[test]
    fn full_range_white() {
        assert!(close(first_pixel(&solid(255, 128, 128, true, ColorMatrix::Bt601)), [255, 255, 255, 255]));
    }

    #[test]
    fn bt709_red_is_bgra_ordered() {
        // BT.709 limited-range pure red: Y=63, U=102, V=240.
        let px = first_pixel(&solid(63, 102, 240, false, ColorMatrix::Bt709));
        assert!(close(px, [0, 0, 255, 255]), "got {px:?}");
    }

    #[test]
    fn grayscale_layout() {
        let mut f = solid(235, 0, 0, false, ColorMatrix::Bt709);
        f.layout = PixelLayout::I400;
        f.planes[1].clear();
        f.planes[2].clear();
        assert!(close(first_pixel(&f), [255, 255, 255, 255]));
    }
}
