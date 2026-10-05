//! Channel count conversion. Multichannel input must be in WAVE order
//! (FL, FR, FC, LFE, BL, BR, ...), which decoders normalize to.

const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Converts interleaved samples from `from` to `to` channels.
pub(crate) fn remix(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
    let (from, to) = (from.max(1) as usize, to.max(1) as usize);
    if from == to {
        return samples.to_vec();
    }
    let frames = samples.chunks_exact(from);
    let mut out = Vec::with_capacity(frames.len() * to);
    for f in frames {
        match (from, to) {
            (1, _) => {
                out.push(f[0]);
                out.push(if to > 1 { f[0] } else { 0.0 });
                out.extend(std::iter::repeat_n(0.0, to.saturating_sub(2)));
            }
            (_, 1) => out.push(f.iter().sum::<f32>() / from as f32),
            (6, 2) => {
                // ITU-R BS.775 downmix without LFE, normalized so a full-scale input cannot clip.
                let norm = 1.0 / (1.0 + 2.0 * MINUS_3DB);
                out.push((f[0] + MINUS_3DB * f[2] + MINUS_3DB * f[4]) * norm);
                out.push((f[1] + MINUS_3DB * f[2] + MINUS_3DB * f[5]) * norm);
            }
            _ => {
                out.extend((0..to).map(|c| f.get(c).copied().unwrap_or(0.0)));
            }
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
}
