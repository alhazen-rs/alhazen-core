//! CPU HDR → SDR conversion: 10-bit PQ/HLG Y′CbCr to tone-mapped BGRA.
//!
//! Per pixel: Y′CbCr → R′G′B′ (f32), transfer curve to nits (lookup table for PQ, formula for
//! HLG), the BT.2390 EETF on max(R, G, B) (lookup table), gamut matrix, sRGB encode (lookup
//! table). The tables are interpolated and built once per (transfer, peak); `tonemap.rs` is
//! the definition they follow.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use rayon::prelude::*;

use crate::color::tonemap::{self, BT2020_TO_BT709, SDR_WHITE_NITS};
use crate::color::{ColorMatrix, Primaries, Transfer};
use crate::decode::{PixelLayout, YuvFrame, YuvPlanes};
use crate::{Error, Result};

const LUT: usize = 4096;

/// Tables by (transfer, peak in whole nits).
type LutCache = Mutex<HashMap<(Transfer, u32), Arc<Luts>>>;

/// Lookup tables for one transfer function and source peak.
struct Luts {
    /// PQ signal (0–1) → nits.
    pq: Vec<f32>,
    /// t = (nits / peak)^(1/4) → eetf(nits) / nits / SDR white (the scale for every channel).
    ratio: Vec<f32>,
    /// s = sqrt(linear) → sRGB-encoded value.
    srgb: Vec<f32>,
    peak: f32,
    /// HLG system gamma for `peak`.
    hlg_gamma: f32,
}

impl Luts {
    fn new(peak: f64) -> Luts {
        let at = |i: usize| i as f64 / (LUT - 1) as f64;
        let ratio = (0..LUT)
            .map(|i| {
                let nits = at(i).powi(4) * peak;
                if nits <= 0.0 { 1.0 / SDR_WHITE_NITS } else { tonemap::eetf(nits, peak) / nits / SDR_WHITE_NITS }
            })
            .map(|v| v as f32)
            .collect();
        Luts {
            pq: (0..LUT).map(|i| tonemap::pq_eotf(at(i)) as f32).collect(),
            ratio,
            srgb: (0..LUT).map(|i| tonemap::srgb_encode(at(i) * at(i)) as f32).collect(),
            peak: peak as f32,
            hlg_gamma: (1.2 * 1.111f64.powf((peak / 1000.0).log2())) as f32,
        }
    }

    fn cached(transfer: Transfer, peak: f64) -> Arc<Luts> {
        static CACHE: OnceLock<LutCache> = OnceLock::new();
        let key = (transfer, peak.round() as u32);
        let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
        cache.entry(key).or_insert_with(|| Arc::new(Luts::new(peak))).clone()
    }
}

/// Linear interpolation in a table over 0–1.
#[inline]
fn lookup(table: &[f32], x: f32) -> f32 {
    let pos = x.clamp(0.0, 1.0) * (LUT - 1) as f32;
    let i = (pos as usize).min(LUT - 2);
    let f = pos - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

#[inline]
fn hlg_inverse_oetf(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 1.0 - 4.0 * A;
    const C: f32 = 0.559_910_7;
    if e <= 0.5 { e * e / 3.0 } else { (((e - C) / A).exp() + B) / 12.0 }
}

/// Y′CbCr → R′G′B′ coefficients (kr·V, kg·U, kg·V, kb·U) for a matrix.
fn coefficients(matrix: ColorMatrix) -> [f32; 4] {
    match matrix {
        ColorMatrix::Bt601 => [1.402, 0.344_136, 0.714_136, 1.772],
        ColorMatrix::Bt709 => [1.5748, 0.187_324, 0.468_124, 1.8556],
        ColorMatrix::Bt2020 => [1.4746, 0.164_553, 0.571_353, 1.8814],
    }
}

/// A PQ or HLG frame with 16-bit (or 8-bit) planes → SDR BGRA, tone mapped. Runs on the current
/// rayon pool.
pub fn hdr_to_bgra(frame: &YuvFrame, out: &mut Vec<u8>) -> Result<()> {
    let widened;
    let planes = match &frame.planes {
        YuvPlanes::U16(p) => p,
        YuvPlanes::U8(p) => {
            widened = p.clone().map(|plane| plane.into_iter().map(u16::from).collect::<Vec<u16>>());
            &widened
        }
    };
    let color = frame.color;
    if !color.transfer.is_hdr() {
        return Err(Error::Decode("hdr_to_bgra on an SDR frame".into()));
    }
    let depth = if matches!(frame.planes, YuvPlanes::U8(_)) { 8 } else { frame.bit_depth.max(8) } as i32;
    let max = ((1u32 << depth) - 1) as f32;
    let scale = (1u32 << (depth - 8)) as f32;
    let (y_off, y_div, c_off, c_div) =
        if color.full_range { (0.0, max, (1u32 << (depth - 1)) as f32, max) } else { (16.0 * scale, 219.0 * scale, 128.0 * scale, 224.0 * scale) };
    let [kr, kgu, kgv, kb] = coefficients(color.matrix);
    let peak = tonemap::source_peak(color.hdr.as_ref());
    let luts = Luts::cached(color.transfer, peak);
    let gamut = (color.primaries == Primaries::Bt2020).then(|| BT2020_TO_BT709.map(|r| r.map(|v| v as f32)));
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (sub_x, sub_y) = match frame.layout {
        PixelLayout::I420 => (1, 1),
        PixelLayout::I422 => (1, 0),
        PixelLayout::I444 | PixelLayout::I400 => (0, 0),
    };
    let gray = frame.layout == PixelLayout::I400;
    out.resize(w * h * 4, 0);
    let transfer = color.transfer;
    out.par_chunks_mut(w * 4).enumerate().for_each(|(row, line)| {
        let yrow = &planes[0][row * frame.strides[0]..];
        let crow = row >> sub_y;
        for (col, px) in line.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let y = (yrow[col] as f32 - y_off) / y_div;
            let (u, v) = if gray {
                (0.0, 0.0)
            } else {
                let c = (col >> sub_x) + crow * frame.strides[1];
                ((planes[1][c] as f32 - c_off) / c_div, (planes[2][c] as f32 - c_off) / c_div)
            };
            let rgb = [y + kr * v, y - kgu * u - kgv * v, y + kb * u].map(|c| c.clamp(0.0, 1.0));
            let nits = match transfer {
                Transfer::Pq => rgb.map(|c| lookup(&luts.pq, c)),
                _ => {
                    let scene = rgb.map(hlg_inverse_oetf);
                    let ys = 0.2627 * scene[0] + 0.6780 * scene[1] + 0.0593 * scene[2];
                    let gain = if ys > 0.0 { luts.peak * ys.powf(luts.hlg_gamma - 1.0) } else { 0.0 };
                    scene.map(|c| c * gain)
                }
            };
            let m = nits[0].max(nits[1]).max(nits[2]);
            let ratio = if m <= luts.peak {
                lookup(&luts.ratio, (m / luts.peak).sqrt().sqrt())
            } else {
                SDR_WHITE_NITS as f32 / m / SDR_WHITE_NITS as f32 // above the peak: clipped to white
            };
            let lin = nits.map(|c| c * ratio);
            let lin = match &gamut {
                Some(g) => [0, 1, 2].map(|r| g[r][0] * lin[0] + g[r][1] * lin[1] + g[r][2] * lin[2]),
                None => lin,
            };
            let [r, g, b] = lin.map(|c| (lookup(&luts.srgb, c.clamp(0.0, 1.0).sqrt()) * 255.0 + 0.5) as u8);
            *px = [b, g, r, 255];
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::color::tonemap::{pq_inverse_eotf, tonemap_pixel};
    use crate::color::{ColorInfo, ColorMatrix, HdrMetadata, Primaries, Transfer};
    use crate::decode::{PixelLayout, YuvFrame, YuvPlanes};

    fn flat_u16(y: u16, u: u16, v: u16, w: u32, h: u32, transfer: Transfer, hdr: Option<HdrMetadata>) -> YuvFrame {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        YuvFrame {
            width: w,
            height: h,
            layout: PixelLayout::I420,
            planes: YuvPlanes::U16([vec![y; (w * h) as usize], vec![u; (cw * ch) as usize], vec![v; (cw * ch) as usize]]),
            bit_depth: 10,
            strides: [w as usize, cw as usize, cw as usize],
            color: ColorInfo { matrix: ColorMatrix::Bt2020, full_range: false, primaries: Primaries::Bt2020, transfer, hdr },
            pts: Duration::ZERO,
        }
    }

    /// The f64 reference for a flat 10-bit limited-range BT.2020 pixel, as 8-bit R, G, B.
    fn reference(y: u16, u: u16, v: u16, transfer: Transfer, peak: f64) -> [u8; 3] {
        let (yf, uf, vf) = ((y as f64 - 64.0) / 876.0, (u as f64 - 512.0) / 896.0, (v as f64 - 512.0) / 896.0);
        let r = yf + 1.4746 * vf;
        let g = yf - 0.164_553 * uf - 0.571_353 * vf;
        let b = yf + 1.8814 * uf;
        tonemap_pixel([r, g, b].map(|c| c.clamp(0.0, 1.0)), transfer, Primaries::Bt2020, peak).map(|c| (c * 255.0).round() as u8)
    }

    #[test]
    fn flat_pq_pixels_match_the_reference() {
        for (y, u, v) in [(64, 512, 512), (940, 512, 512), (500, 400, 600), (700, 300, 700), (800, 512, 900)] {
            let f = flat_u16(y, u, v, 16, 16, Transfer::Pq, None);
            let mut out = Vec::new();
            hdr_to_bgra(&f, &mut out).unwrap();
            let want = reference(y, u, v, Transfer::Pq, 1000.0);
            let got = [out[2], out[1], out[0]];
            assert!(got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2), "{y},{u},{v}: {got:?} vs {want:?}");
            assert_eq!(out[3], 255);
        }
    }

    #[test]
    fn flat_hlg_pixels_match_the_reference() {
        for (y, u, v) in [(64, 512, 512), (721, 512, 512), (940, 512, 512), (600, 350, 650)] {
            let f = flat_u16(y, u, v, 8, 8, Transfer::Hlg, None);
            let mut out = Vec::new();
            hdr_to_bgra(&f, &mut out).unwrap();
            let want = reference(y, u, v, Transfer::Hlg, 1000.0);
            let got = [out[2], out[1], out[0]];
            assert!(got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2), "{y},{u},{v}: {got:?} vs {want:?}");
        }
    }

    #[test]
    fn odd_size_hdr_frame_converts() {
        let f = flat_u16(600, 512, 512, 17, 9, Transfer::Hlg, None);
        let mut out = Vec::new();
        hdr_to_bgra(&f, &mut out).unwrap();
        assert_eq!(out.len(), 17 * 9 * 4);
        assert!(out.chunks(4).all(|p| p == &out[..4]), "flat input gives a flat picture, edges included");
    }

    #[test]
    fn mastering_peak_changes_the_curve() {
        // 1000 nits: white for a 1000-nit master, below white for a 4000-nit one.
        let bright = (pq_inverse_eotf(1000.0) * 876.0 + 64.0).round() as u16;
        let green = |hdr| {
            let mut o = Vec::new();
            hdr_to_bgra(&flat_u16(bright, 512, 512, 4, 4, Transfer::Pq, hdr), &mut o).unwrap();
            o[1]
        };
        let m4000 = HdrMetadata { max_luminance: Some(4000.0), ..Default::default() };
        assert!(green(None) >= 254, "the 1000-nit default peak maps 1000 nits to white");
        let g = green(Some(m4000));
        assert!(g < 254, "a 4000-nit master leaves headroom: {g}");
        assert!(g.abs_diff(reference(bright, 512, 512, Transfer::Pq, 4000.0)[1]) <= 2, "{g}");
    }
}
