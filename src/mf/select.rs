//! Which streams the Media Foundation backend takes (platform-independent, so it is tested
//! everywhere through a fake catalogue).

use crate::demux::{Codec, StreamInfo, StreamKind};

/// A codec Media Foundation can decode, identified independently of the Windows GUIDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MfCodec {
    H264,
    Hevc,
    Vp9,
    Av1,
    Aac,
    Mp3,
    Ac3,
    Eac3,
    Alac,
}

impl MfCodec {
    pub fn of(stream: &StreamInfo) -> Option<MfCodec> {
        Some(match (&stream.kind, &stream.codec) {
            (StreamKind::Video, Codec::H264) => MfCodec::H264,
            (StreamKind::Video, Codec::Hevc) => MfCodec::Hevc,
            (StreamKind::Video, Codec::Vp9) => MfCodec::Vp9,
            (StreamKind::Video, Codec::Av1) => MfCodec::Av1,
            (StreamKind::Audio, Codec::Aac) => MfCodec::Aac,
            (StreamKind::Audio, Codec::Mp3) => MfCodec::Mp3,
            (StreamKind::Audio, Codec::Ac3) => MfCodec::Ac3,
            (StreamKind::Audio, Codec::Eac3) => MfCodec::Eac3,
            (StreamKind::Audio, Codec::Alac) => MfCodec::Alac,
            _ => return None,
        })
    }

    /// Codecs our pure-Rust decoders also handle: only worth Media Foundation's hardware
    /// decoder (Windows' software VP9/AV1 decoders are no better than ours).
    fn native_alternative(self) -> bool {
        matches!(self, MfCodec::Vp9 | MfCodec::Av1)
    }
}

/// What decoders are installed (the real one asks `MFTEnumEx`).
pub trait Catalogue {
    /// Whether a decoder exists for `codec`; with `hardware_only`, a GPU one.
    fn has_decoder(&self, codec: MfCodec, hardware_only: bool) -> bool;
}

/// Whether the Media Foundation backend should take `stream`.
pub fn claims(stream: &StreamInfo, catalogue: &dyn Catalogue, prefer_hardware: bool) -> bool {
    let Some(codec) = MfCodec::of(stream) else { return false };
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
    struct Fake(HashSet<(MfCodec, bool)>);
    impl Catalogue for Fake {
        fn has_decoder(&self, codec: MfCodec, hardware_only: bool) -> bool {
            self.0.contains(&(codec, true)) || (!hardware_only && self.0.contains(&(codec, false)))
        }
    }

    fn stream(kind: StreamKind, codec: Codec) -> StreamInfo {
        StreamInfo::new(1, kind, codec)
    }

    #[test]
    fn vp9_and_av1_only_with_a_hardware_decoder_when_hardware_is_preferred() {
        let sw = Fake([(MfCodec::Vp9, false), (MfCodec::Av1, false)].into());
        let hw = Fake([(MfCodec::Vp9, true)].into());
        let vp9 = stream(StreamKind::Video, Codec::Vp9);
        assert!(!claims(&vp9, &sw, true), "software VP9: native is as good");
        assert!(claims(&vp9, &hw, true));
        assert!(!claims(&stream(StreamKind::Video, Codec::Av1), &hw, true), "no AV1 decoder at all");
        assert!(claims(&vp9, &sw, false), "native first: MF only as a later fallback");
    }

    #[test]
    fn h264_hevc_and_audio_from_any_decoder() {
        let sw = Fake([(MfCodec::H264, false), (MfCodec::Aac, false), (MfCodec::Ac3, false)].into());
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
}
