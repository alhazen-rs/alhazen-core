//! HDR → SDR: PQ/HLG to display light, BT.2390 EETF to SDR white, BT.2020 → BT.709, sRGB.
//!
//! This is the reference definition: the CPU path uses it through lookup tables and the CUDA
//! kernel (`src/nvdec/kernels/convert.cu`) mirrors it.

use crate::color::{HdrMetadata, Primaries, Transfer};

/// SDR reference white (ITU-R BT.2408): this many nits become 1.0.
pub const SDR_WHITE_NITS: f64 = 203.0;
/// Source peak when the stream gives no mastering display or content light level.
pub const DEFAULT_PEAK_NITS: f64 = 1000.0;

const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 4096.0 * 128.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 4096.0 * 32.0;
const C3: f64 = 2392.0 / 4096.0 * 32.0;

/// SMPTE ST 2084: PQ signal (0–1) to nits (0–10000).
pub fn pq_eotf(v: f64) -> f64 {
    let e = v.clamp(0.0, 1.0).powf(1.0 / M2);
    10000.0 * ((e - C1).max(0.0) / (C2 - C3 * e)).powf(1.0 / M1)
}

/// Nits to PQ signal.
pub fn pq_inverse_eotf(nits: f64) -> f64 {
    let y = (nits / 10000.0).clamp(0.0, 1.0).powf(M1);
    ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2)
}

const HLG_A: f64 = 0.178_832_77;
const HLG_B: f64 = 1.0 - 4.0 * HLG_A;
const HLG_C: f64 = 0.559_910_73; // 0.5 − a·ln(4a)

fn hlg_inverse_oetf(e: f64) -> f64 {
    if e <= 0.5 { e * e / 3.0 } else { (((e - HLG_C) / HLG_A).exp() + HLG_B) / 12.0 }
}

/// ITU-R BT.2100 HLG: signal to display nits on a display of `peak` nits (inverse OETF, then
/// the OOTF with its system gamma 1.2 · 1.111^log2(peak / 1000)).
pub fn hlg_to_nits(rgb: [f64; 3], peak: f64) -> [f64; 3] {
    let scene = rgb.map(|e| hlg_inverse_oetf(e.clamp(0.0, 1.0)));
    let gamma = 1.2 * 1.111f64.powf((peak / 1000.0).log2());
    let ys = 0.2627 * scene[0] + 0.6780 * scene[1] + 0.0593 * scene[2];
    let gain = if ys > 0.0 { peak * ys.powf(gamma - 1.0) } else { 0.0 };
    scene.map(|c| c * gain)
}

/// The brightest level the stream was mastered for: mastering display maximum, else MaxCLL,
/// else 1000 nits. Values not above SDR white are not HDR peaks (wrong units, bogus tags) and
/// count as missing.
pub fn source_peak(hdr: Option<&HdrMetadata>) -> f64 {
    let plausible = |v: f64| v > SDR_WHITE_NITS;
    hdr.and_then(|h| {
        h.max_luminance.map(f64::from).filter(|&v| plausible(v)).or(h.max_cll.map(f64::from).filter(|&v| plausible(v)))
    })
    .unwrap_or(DEFAULT_PEAK_NITS)
}

/// ITU-R BT.2390 EETF (without black lift), in the PQ domain: maps `source_peak` to SDR white
/// with a Hermite roll-off above the knee; levels below the knee are unchanged.
pub fn eetf(nits: f64, source_peak: f64) -> f64 {
    let src_max = pq_inverse_eotf(source_peak.max(SDR_WHITE_NITS));
    let e1 = (pq_inverse_eotf(nits) / src_max).clamp(0.0, 1.0);
    let max_lum = pq_inverse_eotf(SDR_WHITE_NITS) / src_max;
    let ks = 1.5 * max_lum - 0.5;
    if ks >= 1.0 {
        // The peak is at SDR white: nothing to roll off, levels above it clip.
        return nits.min(SDR_WHITE_NITS);
    }
    let e2 = if e1 < ks {
        e1
    } else {
        let t = (e1 - ks) / (1.0 - ks);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks) + (-2.0 * t3 + 3.0 * t2) * max_lum
    };
    pq_eotf(e2 * src_max)
}

/// Linear BT.2020 RGB to linear BT.709 RGB.
pub const BT2020_TO_BT709: [[f64; 3]; 3] =
    [[1.660_491, -0.587_641, -0.072_850], [-0.124_550, 1.132_900, -0.008_349], [-0.018_151, -0.100_579, 1.118_730]];

/// The sRGB transfer function, for linear light 0–1.
pub fn srgb_encode(l: f64) -> f64 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 { 12.92 * l } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 }
}

/// One pixel of non-linear R′G′B′ (0–1, already converted from Y′CbCr) to SDR sRGB (0–1).
/// `peak` is the source peak (`source_peak`) for PQ, and the display peak for HLG.
pub fn tonemap_pixel(rgb: [f64; 3], transfer: Transfer, primaries: Primaries, peak: f64) -> [f64; 3] {
    let nits = match transfer {
        Transfer::Pq => rgb.map(pq_eotf),
        Transfer::Hlg => hlg_to_nits(rgb, peak),
        Transfer::Sdr => return rgb,
    };
    let max = nits[0].max(nits[1]).max(nits[2]);
    let scale = if max > 0.0 { eetf(max, peak) / max } else { 0.0 };
    let linear = nits.map(|c| c * scale / SDR_WHITE_NITS);
    let linear = if primaries == Primaries::Bt2020 {
        let m = BT2020_TO_BT709;
        [0, 1, 2].map(|r| m[r][0] * linear[0] + m[r][1] * linear[1] + m[r][2] * linear[2])
    } else {
        linear
    };
    linear.map(srgb_encode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn pq_round_trips_and_hits_reference_points() {
        assert!(close(pq_eotf(0.0), 0.0, 1e-9));
        assert!(close(pq_eotf(1.0), 10000.0, 1e-6));
        assert!(close(pq_eotf(pq_inverse_eotf(100.0)), 100.0, 1e-6));
        // ST 2084: 100 nits ≈ 0.5081 signal, 1000 nits ≈ 0.7518.
        assert!(close(pq_inverse_eotf(100.0), 0.5081, 1e-3));
        assert!(close(pq_inverse_eotf(1000.0), 0.7518, 1e-3));
    }

    #[test]
    fn eetf_keeps_low_levels_and_maps_peak_to_sdr_white() {
        // Below the knee the curve is the identity: 50 nits stays 50 nits.
        assert!(close(eetf(50.0, 1000.0), 50.0, 0.5));
        assert!(close(eetf(0.0, 1000.0), 0.0, 1e-9));
        // The source peak lands on SDR white.
        assert!(close(eetf(1000.0, 1000.0), SDR_WHITE_NITS, 0.5));
        assert!(close(eetf(4000.0, 4000.0), SDR_WHITE_NITS, 0.5));
        let mut last = 0.0;
        for n in (0..=1000).step_by(10) {
            let v = eetf(n as f64, 1000.0);
            assert!(v >= last, "monotonic at {n}");
            last = v;
        }
    }

    #[test]
    fn source_peak_prefers_mastering_then_cll_then_default() {
        let m = HdrMetadata { max_luminance: Some(4000.0), max_cll: Some(1500), ..Default::default() };
        assert_eq!(source_peak(Some(&m)), 4000.0);
        let c = HdrMetadata { max_cll: Some(1500), ..Default::default() };
        assert_eq!(source_peak(Some(&c)), 1500.0);
        assert_eq!(source_peak(None), DEFAULT_PEAK_NITS);
    }

    #[test]
    fn gamut_matrix_keeps_white() {
        for row in BT2020_TO_BT709 {
            assert!(close(row.iter().sum::<f64>(), 1.0, 1e-3));
        }
    }

    #[test]
    fn pq_black_white_and_grey() {
        let px = |nits: f64| {
            let s = pq_inverse_eotf(nits);
            tonemap_pixel([s; 3], Transfer::Pq, Primaries::Bt2020, 1000.0)
        };
        assert!(px(0.0).iter().all(|&c| c < 1e-6), "black stays black");
        assert!(px(1000.0).iter().all(|&c| close(c, 1.0, 2e-3)), "peak → white");
        // 18% of SDR white (36.5 nits) is below the knee: it stays 18% linear.
        assert!(px(0.18 * SDR_WHITE_NITS).iter().all(|&c| close(c, srgb_encode(0.18), 5e-3)), "mid-grey preserved");
    }

    #[test]
    fn hlg_reference_white_is_203_nits_and_maps_like_pq() {
        // HLG's 75% signal is reference white: 203 nits on a 1000-nit display (BT.2408).
        let nits = hlg_to_nits([0.75; 3], 1000.0);
        assert!(nits.iter().all(|&n| close(n, SDR_WHITE_NITS, 3.0)), "{nits:?}");
        // From there it follows the same curve as PQ content at that level and peak (the EETF
        // compresses levels near the knee to fit the 1000-nit peak under SDR white).
        let hlg = tonemap_pixel([0.75; 3], Transfer::Hlg, Primaries::Bt2020, 1000.0);
        let pq = tonemap_pixel([pq_inverse_eotf(nits[0]); 3], Transfer::Pq, Primaries::Bt2020, 1000.0);
        assert!(hlg.iter().zip(pq).all(|(a, b)| close(*a, b, 1e-3)), "{hlg:?} vs {pq:?}");
    }

    #[test]
    fn pq_without_metadata_uses_1000_nits() {
        let s = pq_inverse_eotf(1000.0);
        let out = tonemap_pixel([s; 3], Transfer::Pq, Primaries::Bt2020, source_peak(None));
        assert!(out.iter().all(|&c| c.is_finite() && close(c, 1.0, 2e-3)), "{out:?}");
    }

    #[test]
    fn implausible_peaks_are_ignored_and_never_blacken_highlights() {
        // MaxCLL 100 or a mastering maximum in the wrong units: not an HDR peak.
        let bogus = HdrMetadata { max_luminance: Some(150.0), max_cll: Some(100), ..Default::default() };
        assert_eq!(source_peak(Some(&bogus)), DEFAULT_PEAK_NITS);
        // Even when called with such a peak, bright pixels clip to white, not black or NaN.
        for nits in [203.0, 500.0, 4000.0] {
            let out = eetf(nits, 150.0);
            assert!(out.is_finite() && close(out, SDR_WHITE_NITS, 0.5), "{nits} nits → {out}");
        }
    }
}
