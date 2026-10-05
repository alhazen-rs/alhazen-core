//! Channel count conversion. Multichannel input must be in WAVE order
//! (FL, FR, FC, LFE, BL, BR, SL, SR), which decoders normalize to.

const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// (left, right) weight of each WAVE-order channel when folding a layout down to stereo
/// (ITU-R BS.775: centre and surrounds at -3 dB, LFE dropped).
fn stereo_weights(channels: usize) -> Option<&'static [(f32, f32)]> {
    const FL: (f32, f32) = (1.0, 0.0);
    const FR: (f32, f32) = (0.0, 1.0);
    const FC: (f32, f32) = (MINUS_3DB, MINUS_3DB);
    const LFE: (f32, f32) = (0.0, 0.0);
    const SL: (f32, f32) = (MINUS_3DB, 0.0);
    const SR: (f32, f32) = (0.0, MINUS_3DB);
    const BC: (f32, f32) = (0.5, 0.5);
    Some(match channels {
        3 => &[FL, FR, FC],
        4 => &[FL, FR, SL, SR],                      // quad: FL FR BL BR
        5 => &[FL, FR, FC, SL, SR],                  // 5.0: FL FR FC BL BR
        6 => &[FL, FR, FC, LFE, SL, SR],             // 5.1
        7 => &[FL, FR, FC, LFE, BC, SL, SR],         // 6.1: ... BC SL SR
        8 => &[FL, FR, FC, LFE, SL, SR, SL, SR],     // 7.1: ... BL BR SL SR
        _ => return None,
    })
}

/// Converts interleaved samples from `from` to `to` channels.
pub(crate) fn remix(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
    let (from, to) = (from.max(1) as usize, to.max(1) as usize);
    if from == to {
        return samples.to_vec();
    }
    let weights = (to == 2).then(|| stereo_weights(from)).flatten();
    // Normalized so a full-scale signal on every channel cannot clip.
    let norm = weights.map(|w| {
        let (l, r) = w.iter().fold((0.0, 0.0), |(l, r), (a, b)| (l + a, r + b));
        1.0 / f32::max(l, r).max(1.0)
    });
    let frames = samples.chunks_exact(from);
    let mut out = Vec::with_capacity(frames.len() * to);
    for f in frames {
        if let (Some(w), Some(norm)) = (weights, norm) {
            let (l, r) = f.iter().zip(w).fold((0.0, 0.0), |(l, r), (s, (a, b))| (l + s * a, r + s * b));
            out.push(l * norm);
            out.push(r * norm);
            continue;
        }
        match (from, to) {
            (1, _) => {
                out.push(f[0]);
                out.push(if to > 1 { f[0] } else { 0.0 });
                out.extend(std::iter::repeat_n(0.0, to.saturating_sub(2)));
            }
            (_, 1) => out.push(f.iter().sum::<f32>() / from as f32),
            _ => out.extend((0..to).map(|c| f.get(c).copied().unwrap_or(0.0))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_to_stereo_duplicates_and_stereo_to_mono_averages() {
        assert_eq!(remix(&[0.5, -0.5], 1, 2), vec![0.5, 0.5, -0.5, -0.5]);
        assert_eq!(remix(&[0.2, 0.4], 2, 1), vec![0.3]);
        assert_eq!(remix(&[0.1, 0.2], 2, 2), vec![0.1, 0.2]);
    }

    #[test]
    fn surround_downmix_keeps_left_right_and_never_clips() {
        // FL only -> left only.
        let out = remix(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, 2);
        assert!(out[0] > 0.0 && out[1] == 0.0);
        // LFE is dropped.
        assert_eq!(remix(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0], 6, 2), vec![0.0, 0.0]);
        // Everything at full scale stays within [-1, 1].
        let out = remix(&[1.0; 6], 6, 2);
        assert!(out.iter().all(|s| s.abs() <= 1.0 + 1e-6), "{out:?}");
    }

    #[test]
    fn stereo_to_surround_fills_front_pair() {
        assert_eq!(remix(&[0.1, 0.2], 2, 6), vec![0.1, 0.2, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn every_surround_layout_keeps_the_centre_balanced() {
        // WAVE order: the centre (dialogue) is channel 2 in all of these.
        for ch in [3usize, 5, 6, 7, 8] {
            let mut f = vec![0.0; ch];
            f[2] = 1.0;
            let out = remix(&f, ch as u16, 2);
            assert!(out[0] > 0.2 && (out[0] - out[1]).abs() < 1e-6, "{ch} channels: {out:?}");
            let full = remix(&vec![1.0; ch], ch as u16, 2);
            assert!(full.iter().all(|s| s.abs() <= 1.0 + 1e-6), "{ch} channels clip: {full:?}");
        }
        // 7.1: side-left reaches the left speaker only.
        let mut f = vec![0.0; 8];
        f[6] = 1.0;
        let out = remix(&f, 8, 2);
        assert!(out[0] > 0.0 && out[1] == 0.0, "{out:?}");
    }
}
