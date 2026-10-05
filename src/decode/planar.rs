//! Helpers for decoders that produce `u16` planes: packing to 8 bits and 4:4:0 chroma.

/// Packs the visible `w`×`h` samples of a `u16` plane (`stride` samples per row) into tightly
/// packed 8-bit rows, dropping the `bit_depth - 8` low bits.
pub(crate) fn pack_8bit(src: &[u16], stride: usize, w: usize, h: usize, bit_depth: u32) -> Vec<u8> {
    let shift = bit_depth.saturating_sub(8);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        out.extend(src[y * stride..y * stride + w].iter().map(|&s| (s >> shift).min(255) as u8));
    }
    out
}

/// Repeats every row of a `w`-wide plane twice, keeping `height` rows: 4:4:0 chroma (half
/// height) becomes 4:4:4.
pub(crate) fn double_rows(plane: &[u8], w: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * height);
    for y in 0..height {
        let src = (y / 2) * w;
        out.extend_from_slice(&plane[src..src + w]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_drops_low_bits_and_stride_padding() {
        // 2x2 visible inside a stride of 3; 10-bit.
        let src = [1023u16, 4, 999, 512, 516, 999];
        assert_eq!(pack_8bit(&src, 3, 2, 2, 10), [255, 1, 128, 129]);
        assert_eq!(pack_8bit(&[200u16, 7], 2, 2, 1, 8), [200, 7]);
    }

    #[test]
    fn pack_clamps_out_of_range_samples() {
        assert_eq!(pack_8bit(&[300u16], 1, 1, 1, 8), [255]);
    }

    #[test]
    fn double_rows_repeats_each_row() {
        assert_eq!(double_rows(&[1, 2, 3, 4], 2, 4), [1, 2, 1, 2, 3, 4, 3, 4]);
        assert_eq!(double_rows(&[1, 2, 3, 4], 2, 3), [1, 2, 1, 2, 3, 4], "odd heights keep the last row once");
    }
}
