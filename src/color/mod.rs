//! Colour description of decoded pictures: matrix, range, primaries, transfer, HDR metadata.

use crate::demux::StreamInfo;
use crate::nal::VuiColor;

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
    /// An SDR picture with the primaries that usually go with its matrix.
    pub fn sdr(matrix: ColorMatrix, full_range: bool) -> ColorInfo {
        let primaries = match matrix {
            ColorMatrix::Bt601 => Primaries::Bt601,
            ColorMatrix::Bt709 => Primaries::Bt709,
            ColorMatrix::Bt2020 => Primaries::Bt2020,
        };
        ColorInfo { matrix, full_range, primaries, transfer: Transfer::Sdr, hdr: None }
    }

    /// A stream's colour description: each value from the container, else from the bitstream
    /// (`vui`), else the usual guess (BT.709 from 720p up, BT.601 below; limited range; SDR).
    /// HDR metadata is kept only for PQ/HLG.
    pub fn resolve(stream: &StreamInfo, vui: Option<VuiColor>, height: u32) -> ColorInfo {
        let matrix = stream
            .color_matrix
            .and_then(ColorMatrix::from_h273)
            .or_else(|| vui.and_then(|v| ColorMatrix::from_h273(v.matrix)))
            .unwrap_or_else(|| ColorMatrix::guess_for_height(height));
        let full_range = stream.full_range.or(vui.map(|v| v.full_range)).unwrap_or(false);
        let mut color = ColorInfo::sdr(matrix, full_range);
        if let Some(p) = stream.primaries.and_then(Primaries::from_h273).or_else(|| vui.and_then(|v| Primaries::from_h273(v.primaries))) {
            color.primaries = p;
        }
        color.transfer = stream
            .transfer
            .and_then(Transfer::from_h273)
            .or_else(|| vui.and_then(|v| Transfer::from_h273(v.transfer)))
            .unwrap_or(Transfer::Sdr);
        color.hdr = stream.hdr.filter(|_| color.transfer.is_hdr());
        color
    }

    /// Applies what the stream signals (container tags, else the SPS VUI) over what a decoder
    /// found in the picture: the container wins, the decoder's own bitstream comes next.
    pub fn apply_stream(&mut self, signalled: &StreamColor) {
        if let Some(p) = signalled.primaries {
            self.primaries = p;
        }
        if let Some(t) = signalled.transfer {
            self.transfer = t;
        }
        if signalled.hdr.is_some() {
            self.hdr = signalled.hdr;
        }
        if !self.transfer.is_hdr() {
            self.hdr = None;
        }
    }
}

/// The primaries, transfer and HDR metadata a stream signals outside the pictures: container
/// tags, else its SPS VUI. `None` for what it does not say.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StreamColor {
    pub primaries: Option<Primaries>,
    pub transfer: Option<Transfer>,
    pub hdr: Option<HdrMetadata>,
}

impl StreamColor {
    pub fn of(stream: &StreamInfo) -> StreamColor {
        let vui = stream.extradata.as_deref().and_then(|c| crate::nal::vui_color(&stream.codec, c));
        StreamColor {
            primaries: stream.primaries.and_then(Primaries::from_h273).or_else(|| vui.and_then(|v| Primaries::from_h273(v.primaries))),
            transfer: stream.transfer.and_then(Transfer::from_h273).or_else(|| vui.and_then(|v| Transfer::from_h273(v.transfer))),
            hdr: stream.hdr,
        }
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

    use crate::demux::{Codec, StreamKind};

    #[test]
    fn container_wins_then_bitstream_then_guess() {
        let mut s = StreamInfo::new(0, StreamKind::Video, Codec::Hevc);
        let vui = VuiColor { primaries: 9, transfer: 16, matrix: 9, full_range: false };
        let c = ColorInfo::resolve(&s, Some(vui), 2160);
        assert_eq!((c.primaries, c.transfer, c.matrix), (Primaries::Bt2020, Transfer::Pq, ColorMatrix::Bt2020));
        s.transfer = Some(1);
        assert_eq!(ColorInfo::resolve(&s, Some(vui), 2160).transfer, Transfer::Sdr);
        let bare = StreamInfo::new(0, StreamKind::Video, Codec::Hevc);
        let g = ColorInfo::resolve(&bare, None, 480);
        assert_eq!((g.matrix, g.primaries, g.transfer, g.full_range), (ColorMatrix::Bt601, Primaries::Bt601, Transfer::Sdr, false));
    }

    #[test]
    fn hdr_metadata_kept_only_for_hdr_transfers() {
        let mut s = StreamInfo::new(0, StreamKind::Video, Codec::Hevc);
        s.transfer = Some(1);
        s.hdr = Some(HdrMetadata { max_cll: Some(1000), ..Default::default() });
        assert_eq!(ColorInfo::resolve(&s, None, 1080).hdr, None);
        s.transfer = Some(16);
        assert_eq!(ColorInfo::resolve(&s, None, 1080).hdr.and_then(|h| h.max_cll), Some(1000));
    }
}
