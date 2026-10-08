//! Which streams a hardware backend (Media Foundation, NVDEC) takes. Platform-independent, so it
//! is tested everywhere through a fake catalogue.

use crate::demux::{Codec, StreamInfo, StreamKind};

/// A codec a platform decoder can decode, independent of each platform's identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HwCodec {
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
    Aac,
    Mp3,
    Ac3,
    Eac3,
    Alac,
}

impl HwCodec {
    pub fn of(stream: &StreamInfo) -> Option<HwCodec> {
        Some(match (&stream.kind, &stream.codec) {
            (StreamKind::Video, Codec::H264) => HwCodec::H264,
            (StreamKind::Video, Codec::Hevc) => HwCodec::Hevc,
            (StreamKind::Video, Codec::Vp8) => HwCodec::Vp8,
            (StreamKind::Video, Codec::Vp9) => HwCodec::Vp9,
            (StreamKind::Video, Codec::Av1) => HwCodec::Av1,
            (StreamKind::Audio, Codec::Aac) => HwCodec::Aac,
            (StreamKind::Audio, Codec::Mp3) => HwCodec::Mp3,
            (StreamKind::Audio, Codec::Ac3) => HwCodec::Ac3,
            (StreamKind::Audio, Codec::Eac3) => HwCodec::Eac3,
            (StreamKind::Audio, Codec::Alac) => HwCodec::Alac,
            _ => return None,
        })
    }

    /// Codecs our pure-Rust decoders also handle: only worth a hardware decoder.
    fn native_alternative(self) -> bool {
        matches!(self, HwCodec::Vp8 | HwCodec::Vp9 | HwCodec::Av1)
    }
}

/// What decoders the platform has (Media Foundation asks MFTEnumEx; NVDEC asks the GPU).
pub trait Catalogue {
    /// Whether a decoder exists for `codec`; with `hardware_only`, a GPU one.
    fn has_decoder(&self, codec: HwCodec, hardware_only: bool) -> bool;
}

/// Whether a hardware backend should take `stream`.
pub fn claims(stream: &StreamInfo, catalogue: &dyn Catalogue, prefer_hardware: bool) -> bool {
    let Some(codec) = HwCodec::of(stream) else { return false };
    if codec.native_alternative() && prefer_hardware {
        catalogue.has_decoder(codec, true)
    } else {
        catalogue.has_decoder(codec, false)
    }
}

/// Backend priority: ahead of `native` (0) when hardware is preferred, else between `native`
/// and `ffmpeg-cli` (-10).
pub fn priority(prefer_hardware: bool) -> i32 {
    if prefer_hardware { 10 } else { -5 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Installed decoders: (codec, is_hardware).
    struct Fake(HashSet<(HwCodec, bool)>);
    impl Catalogue for Fake {
        fn has_decoder(&self, codec: HwCodec, hardware_only: bool) -> bool {
            self.0.contains(&(codec, true)) || (!hardware_only && self.0.contains(&(codec, false)))
        }
    }

    fn stream(kind: StreamKind, codec: Codec) -> StreamInfo {
        StreamInfo::new(1, kind, codec)
    }

    #[test]
    fn vp9_and_av1_only_with_a_hardware_decoder_when_hardware_is_preferred() {
        let sw = Fake([(HwCodec::Vp9, false), (HwCodec::Av1, false)].into());
        let hw = Fake([(HwCodec::Vp9, true)].into());
        let vp9 = stream(StreamKind::Video, Codec::Vp9);
        assert!(!claims(&vp9, &sw, true), "software VP9: native is as good");
        assert!(claims(&vp9, &hw, true));
        assert!(!claims(&stream(StreamKind::Video, Codec::Av1), &hw, true), "no AV1 decoder at all");
        assert!(claims(&vp9, &sw, false), "native first: MF only as a later fallback");
    }

    #[test]
    fn h264_hevc_and_audio_from_any_decoder() {
        let sw = Fake([(HwCodec::H264, false), (HwCodec::Aac, false), (HwCodec::Ac3, false)].into());
        assert!(claims(&stream(StreamKind::Video, Codec::H264), &sw, true));
        assert!(claims(&stream(StreamKind::Audio, Codec::Aac), &sw, true));
        assert!(claims(&stream(StreamKind::Audio, Codec::Ac3), &sw, false));
        assert!(!claims(&stream(StreamKind::Video, Codec::Hevc), &sw, true), "HEVC extension not installed");
        assert!(!claims(&stream(StreamKind::Audio, Codec::Opus), &sw, true), "not a Media Foundation codec");
        assert!(!claims(&stream(StreamKind::Audio, Codec::H264), &sw, true), "kind must match");
    }

    #[test]
    fn priority_follows_the_preference() {
        assert!(priority(true) > 0, "ahead of native");
        assert!(priority(false) < 0 && priority(false) > -10, "between native and ffmpeg-cli");
    }

    #[test]
    fn vp8_behaves_like_vp9_and_av1() {
        let hw = Fake([(HwCodec::Vp8, true)].into());
        let sw = Fake([(HwCodec::Vp8, false)].into());
        let vp8 = stream(StreamKind::Video, Codec::Vp8);
        assert_eq!(HwCodec::of(&vp8), Some(HwCodec::Vp8));
        assert!(claims(&vp8, &hw, true));
        assert!(!claims(&vp8, &sw, true), "software VP8: native is as good");
    }
}
