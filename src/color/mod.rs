//! Colour description of decoded pictures: matrix, range, primaries, transfer, HDR metadata.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl ColorMatrix {
    /// From an ITU-T H.273 MatrixCoefficients value (as in containers and codec headers).
    pub fn from_h273(m: u8) -> Option<ColorMatrix> {
        match m {
            1 => Some(ColorMatrix::Bt709),
            5 | 6 => Some(ColorMatrix::Bt601),
            9 | 10 => Some(ColorMatrix::Bt2020),
            _ => None,
        }
    }

    /// Fallback when the stream does not say: HD and larger is BT.709, SD is BT.601.
    pub fn guess_for_height(height: u32) -> ColorMatrix {
        if height >= 720 { ColorMatrix::Bt709 } else { ColorMatrix::Bt601 }
    }
}

/// Colour primaries (ITU-T H.273 ColourPrimaries); others map to the nearest of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primaries {
    Bt709,
    Bt601,
    Bt2020,
}

impl Primaries {
    pub fn from_h273(code: u8) -> Option<Primaries> {
        match code {
            1 => Some(Primaries::Bt709),
            5 | 6 => Some(Primaries::Bt601),
            9 => Some(Primaries::Bt2020),
            _ => None,
        }
    }
}

/// Transfer function: SDR gamma, PQ (SMPTE ST 2084) or HLG (ARIB STD-B67).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Transfer {
    #[default]
    Sdr,
    Pq,
    Hlg,
}

impl Transfer {
    /// ITU-T H.273 TransferCharacteristics: 16 PQ, 18 HLG; the other specified curves are SDR.
    pub fn from_h273(code: u8) -> Option<Transfer> {
        match code {
            16 => Some(Transfer::Pq),
            18 => Some(Transfer::Hlg),
            1 | 4..=8 | 11..=15 => Some(Transfer::Sdr),
            _ => None,
        }
    }

    pub fn is_hdr(self) -> bool {
        self != Transfer::Sdr
    }
}

/// HDR10 static metadata: mastering display colour volume and content light levels.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct HdrMetadata {
    /// Mastering display primaries, R, G, B as CIE 1931 (x, y).
    pub display_primaries: Option<[(f32, f32); 3]>,
    pub white_point: Option<(f32, f32)>,
    /// Mastering display luminance in nits.
    pub max_luminance: Option<f32>,
    pub min_luminance: Option<f32>,
    /// Maximum content and frame-average light levels in nits.
    pub max_cll: Option<u16>,
    pub max_fall: Option<u16>,
}

/// Everything needed to turn a picture's samples into colours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorInfo {
    pub matrix: ColorMatrix,
    pub full_range: bool,
    pub primaries: Primaries,
    pub transfer: Transfer,
    pub hdr: Option<HdrMetadata>,
}

impl ColorInfo {
    /// An SDR picture with BT.709 primaries (BT.601 primaries for a BT.601 matrix).
    pub fn sdr(matrix: ColorMatrix, full_range: bool) -> ColorInfo {
        let primaries = if matrix == ColorMatrix::Bt601 { Primaries::Bt601 } else { Primaries::Bt709 };
        ColorInfo { matrix, full_range, primaries, transfer: Transfer::Sdr, hdr: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h273_transfer_codes() {
        assert_eq!(Transfer::from_h273(16), Some(Transfer::Pq));
        assert_eq!(Transfer::from_h273(18), Some(Transfer::Hlg));
        assert_eq!(Transfer::from_h273(1), Some(Transfer::Sdr));
        assert_eq!(Transfer::from_h273(2), None); // unspecified
        assert!(Transfer::Pq.is_hdr() && Transfer::Hlg.is_hdr() && !Transfer::Sdr.is_hdr());
    }

    #[test]
    fn h273_primaries_codes() {
        assert_eq!(Primaries::from_h273(1), Some(Primaries::Bt709));
        assert_eq!(Primaries::from_h273(9), Some(Primaries::Bt2020));
        assert_eq!(Primaries::from_h273(5), Some(Primaries::Bt601));
        assert_eq!(Primaries::from_h273(6), Some(Primaries::Bt601));
        assert_eq!(Primaries::from_h273(2), None);
    }
}
