//! Channel order conversion. Vorbis, and Opus mapping family 1, use the Vorbis order;
//! the mixer expects WAVE order.

/// Vorbis channel order (spec §4.3.9) -> WAVE order (FL, FR, FC, LFE, BL, BR, SL, SR).
/// Entry `i` is the Vorbis channel that becomes WAVE channel `i`.
fn wave_map(channels: u16) -> Option<&'static [usize]> {
    Some(match channels {
        3 => &[0, 2, 1],                   // L C R
        5 => &[0, 2, 1, 3, 4],             // FL FC FR BL BR
        6 => &[0, 2, 1, 5, 3, 4],          // FL FC FR BL BR LFE
        7 => &[0, 2, 1, 6, 5, 3, 4],       // FL FC FR SL SR BC LFE
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],    // FL FC FR SL SR BL BR LFE
        _ => return None,
    })
}

pub(crate) fn to_wave_order(samples: Vec<f32>, channels: u16) -> Vec<f32> {
    let Some(map) = wave_map(channels) else { return samples };
    let mut out = Vec::with_capacity(samples.len());
    for frame in samples.chunks_exact(channels as usize) {
        out.extend(map.iter().map(|&src| frame[src]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorders_5_1_to_wave() {
        // Vorbis frame: FL=1 FC=2 FR=3 BL=4 BR=5 LFE=6
        let out = to_wave_order(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6);
        assert_eq!(out, vec![1.0, 3.0, 2.0, 6.0, 4.0, 5.0]);
        assert_eq!(to_wave_order(vec![1.0, 2.0], 2), vec![1.0, 2.0], "stereo untouched");
    }
}
