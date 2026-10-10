//! VideoToolbox's bi-planar pictures (Y, then interleaved Cb/Cr; 8 or 16 bits per sample) as
//! the planar 8-bit I420 our pipeline converts.

/// Y, U and V planes, tightly packed (`w × h`, then two `ceil(w/2) × ceil(h/2)`), from a luma
/// plane and an interleaved chroma plane with their row strides in bytes. With 2 bytes per sample
/// (little-endian, value in the high bits), the high byte is kept.
pub(crate) fn biplanar_to_i420(y: &[u8], y_stride: usize, uv: &[u8], uv_stride: usize, w: u32, h: u32, bytes_per_sample: usize) -> [Vec<u8>; 3] {
    let (w, h) = (w as usize, h as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let bps = bytes_per_sample.max(1);
    let hi = bps - 1; // the byte to keep
    let mut py = Vec::with_capacity(w * h);
    for row in y.chunks(y_stride).take(h) {
        if bps == 1 {
            py.extend_from_slice(&row[..w.min(row.len())]);
        } else {
            py.extend(row.chunks_exact(bps).take(w).map(|s| s[hi]));
        }
    }
    let (mut pu, mut pv) = (Vec::with_capacity(cw * ch), Vec::with_capacity(cw * ch));
    for row in uv.chunks(uv_stride).take(ch) {
        for pair in row.chunks_exact(2 * bps).take(cw) {
            pu.push(pair[hi]);
            pv.push(pair[bps + hi]);
        }
    }
    [py, pu, pv]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_bit_with_padded_rows() {
        // 4x2 picture, rows padded to 6 bytes; chroma 2x1 pairs.
        let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        let uv = [10, 20, 11, 21, 0, 0];
        let [py, pu, pv] = biplanar_to_i420(&y, 6, &uv, 6, 4, 2, 1);
        assert_eq!(py, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(pu, [10, 11]);
        assert_eq!(pv, [20, 21]);
    }

    #[test]
    fn odd_sizes_round_chroma_up() {
        let (w, h) = (5, 3);
        let y: Vec<u8> = (0..(w * h) as u8).collect();
        let uv: Vec<u8> = (100..112).collect(); // 3x2 pairs, stride 6
        let [py, pu, pv] = biplanar_to_i420(&y, w as usize, &uv, 6, w, h, 1);
        assert_eq!(py.len(), 15);
        assert_eq!(pu, [100, 102, 104, 106, 108, 110]);
        assert_eq!(pv, [101, 103, 105, 107, 109, 111]);
    }

    #[test]
    fn sixteen_bit_keeps_the_high_byte() {
        // Little-endian 16-bit samples with the value in the high bits (P010/x420).
        let y = [0x00, 0x80, 0xC0, 0x40];
        let uv = [0x00, 0x11, 0x00, 0x22];
        let [py, pu, pv] = biplanar_to_i420(&y, 4, &uv, 4, 2, 1, 2);
        assert_eq!(py, [0x80, 0x40]);
        assert_eq!(pu, [0x11]);
        assert_eq!(pv, [0x22]);
    }
}
