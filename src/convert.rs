//! YUV -> BGRA conversion (SIMD via the `yuv` crate, rows split across a rayon pool).

use yuv::{YuvGrayImage, YuvPlanarImage, YuvRange, YuvStandardMatrix};

use crate::decode::{ColorMatrix, PixelLayout, YuvFrame, YuvPlanes};
use crate::{Error, Result};

/// Converts `frame` into tightly packed BGRA (`width * 4` bytes per row).
/// Runs on the current rayon pool; call inside `pool.install` to choose the pool.
pub fn yuv_to_bgra(frame: &YuvFrame, out: &mut Vec<u8>) -> Result<()> {
    let (w, h) = (frame.width, frame.height);
    out.resize(w as usize * h as usize * 4, 0);
    let planes = frame.u8_planes().ok_or_else(|| Error::Decode("16-bit frame on the 8-bit path".into()))?;
    let range = if frame.color.full_range { YuvRange::Full } else { YuvRange::Limited };
    let matrix = match frame.color.matrix {
        ColorMatrix::Bt601 => YuvStandardMatrix::Bt601,
        ColorMatrix::Bt709 => YuvStandardMatrix::Bt709,
        ColorMatrix::Bt2020 => YuvStandardMatrix::Bt2020,
    };
    let stride = w * 4;
    let result = if frame.layout == PixelLayout::I400 {
        let gray = YuvGrayImage {
            y_plane: &planes[0],
            y_stride: frame.strides[0] as u32,
            width: w,
            height: h,
        };
        yuv::yuv400_to_bgra(&gray, out, stride, range, matrix)
    } else {
        let image = YuvPlanarImage {
            y_plane: &planes[0],
            y_stride: frame.strides[0] as u32,
            u_plane: &planes[1],
            u_stride: frame.strides[1] as u32,
            v_plane: &planes[2],
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

/// Any frame to SDR BGRA: PQ/HLG frames are tone mapped (`color::hdr_convert`), SDR frames
/// converted as they are (10-bit ones shifted to 8 bits first).
pub fn frame_to_bgra(frame: &YuvFrame, out: &mut Vec<u8>) -> Result<()> {
    if frame.color.transfer.is_hdr() {
        return crate::color::hdr_convert::hdr_to_bgra(frame, out);
    }
    match frame.planes {
        YuvPlanes::U8(_) => yuv_to_bgra(frame, out),
        YuvPlanes::U16(_) => yuv_to_bgra(&to_8bit(frame), out),
    }
}

/// The frame with 8-bit samples: 16-bit planes are shifted down by `bit_depth - 8`; 8-bit
/// frames come back as a copy.
pub fn to_8bit(frame: &YuvFrame) -> YuvFrame {
    let YuvPlanes::U16(planes) = &frame.planes else { return frame.clone() };
    let shift = frame.bit_depth.saturating_sub(8);
    let narrow = |p: &Vec<u16>| p.iter().map(|&s| (s >> shift).min(255) as u8).collect();
    YuvFrame { planes: YuvPlanes::U8([narrow(&planes[0]), narrow(&planes[1]), narrow(&planes[2])]), bit_depth: 8, ..frame.clone() }
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
            planes: crate::decode::YuvPlanes::U8([vec![y; 16], vec![u; 4], vec![v; 4]]),
            strides: [4, 2, 2],
            bit_depth: 8,
            color: crate::color::ColorInfo::sdr(matrix, full_range),
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
    fn to_8bit_shifts_by_the_bit_depth() {
        let mut f = solid(0, 0, 0, false, ColorMatrix::Bt709);
        f.planes = YuvPlanes::U16([vec![1023; 16], vec![512; 4], vec![4; 4]]);
        f.bit_depth = 10;
        let n = to_8bit(&f);
        assert_eq!((n.bit_depth, n.u8_planes().unwrap()[0][0], n.u8_planes().unwrap()[1][0], n.u8_planes().unwrap()[2][0]), (8, 255, 128, 1));
    }

    #[test]
    fn grayscale_layout() {
        let mut f = solid(235, 0, 0, false, ColorMatrix::Bt709);
        f.layout = PixelLayout::I400;
        if let crate::decode::YuvPlanes::U8(p) = &mut f.planes {
            p[1].clear();
            p[2].clear();
        }
        assert!(close(first_pixel(&f), [255, 255, 255, 255]));
    }
}
