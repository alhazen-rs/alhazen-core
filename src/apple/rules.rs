//! Which streams Apple's decoders take.

use crate::hw::select::HwCodec;

/// Whether Apple's decoders should be asked for `codec` (the `Catalogue` answer). `hardware`
/// tells whether the Mac decodes a codec in hardware (`VTIsHardwareDecodeSupported`).
///
/// H.264 and HEVC: always (VideoToolbox; hardware on every Apple Silicon Mac). AV1, VP9 and
/// ProRes: only in hardware, whatever `_hardware_only` says: the native decoders are as good as
/// VideoToolbox's software ones. Audio: only what the native decoders lack (ALAC, AC-3, E-AC-3,
/// and AAC streams that are USAC, decided per stream with [`is_usac`]).
pub(crate) fn has_decoder(codec: HwCodec, _hardware_only: bool, hardware: impl Fn(HwCodec) -> bool) -> bool {
    match codec {
        HwCodec::H264 | HwCodec::Hevc => true,
        HwCodec::Av1 | HwCodec::Vp9 | HwCodec::ProRes => hardware(codec),
        HwCodec::Alac | HwCodec::Ac3 | HwCodec::Eac3 | HwCodec::Aac => true,
        HwCodec::Vp8 | HwCodec::Mp3 => false,
    }
}

/// Whether the stream carries what Apple's decoders need before the first packet: the setup
/// record for H.264, HEVC and AV1 (VP9's is built from its first keyframe), ALAC's cookie, and for
/// AAC a USAC config (other AAC stays native). Streams without it are left to other backends.
pub(crate) fn stream_ok(stream: &crate::demux::StreamInfo) -> bool {
    use crate::demux::Codec;
    let setup = stream.extradata.as_deref().filter(|d| !d.is_empty());
    match stream.codec {
        Codec::H264 | Codec::Hevc | Codec::Av1 | Codec::Alac => setup.is_some(),
        Codec::Aac => setup.is_some_and(is_usac),
        _ => true,
    }
}

/// Whether an AudioSpecificConfig is USAC (xHE-AAC, audio object type 42), which the native AAC
/// decoder does not handle.
pub(crate) fn is_usac(asc: &[u8]) -> bool {
    let Some(&first) = asc.first() else { return false };
    let aot = first >> 3;
    if aot != 31 {
        return false; // 42 needs the escape
    }
    // Escape: 32 + the next 6 bits.
    let Some(&second) = asc.get(1) else { return false };
    let ext = ((first & 0x07) << 3) | (second >> 5);
    32 + ext == 42
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::select::HwCodec::{self, *};

    fn none(_: HwCodec) -> bool {
        false
    }
    fn all(_: HwCodec) -> bool {
        true
    }

    #[test]
    fn h264_and_hevc_always_others_only_in_hardware() {
        for preferred in [true, false] {
            assert!(has_decoder(H264, preferred, none));
            assert!(has_decoder(Hevc, preferred, none));
            for c in [Av1, Vp9, ProRes] {
                assert!(!has_decoder(c, preferred, none), "{c:?} without hardware: native keeps it");
                assert!(has_decoder(c, preferred, all), "{c:?} with hardware");
            }
        }
    }

    #[test]
    fn only_audio_native_lacks() {
        for c in [Alac, Ac3, Eac3] {
            assert!(has_decoder(c, false, none), "{c:?}");
        }
        for c in [Mp3, Vp8] {
            assert!(!has_decoder(c, false, all), "{c:?} stays native");
        }
        assert!(has_decoder(Aac, false, none), "AAC is decided per stream: USAC only");
    }

    #[test]
    fn usac_is_recognised_from_the_audio_specific_config() {
        // audioObjectType 31 (escape) + 6 bits: 42 - 32 = 10 → 11111 001010.
        assert!(is_usac(&[0xF9, 0x40, 0x00]));
        assert!(!is_usac(&[0x12, 0x10]), "AAC-LC");
        assert!(!is_usac(&[0x2B, 0x11, 0x88, 0x00]), "HE-AAC (SBR)");
        assert!(!is_usac(&[]));
        assert!(!is_usac(&[0xF8]), "truncated escape");
    }

    #[test]
    fn streams_without_what_the_decoder_needs_are_not_claimed() {
        use crate::demux::{Codec, StreamInfo, StreamKind};
        let with = |kind, codec, extradata: Option<Vec<u8>>| {
            let mut s = StreamInfo::new(1, kind, codec);
            s.extradata = extradata;
            s
        };
        for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
            assert!(!stream_ok(&with(StreamKind::Video, c.clone(), None)), "{c:?} without its setup record");
            assert!(stream_ok(&with(StreamKind::Video, c, Some(vec![1, 2, 3]))));
        }
        assert!(stream_ok(&with(StreamKind::Video, Codec::Vp9, None)), "vpcC comes from the first keyframe");
        assert!(stream_ok(&with(StreamKind::Video, Codec::ProRes, None)));
        assert!(!stream_ok(&with(StreamKind::Audio, Codec::Aac, Some(vec![0x12, 0x10]))), "plain AAC");
        assert!(stream_ok(&with(StreamKind::Audio, Codec::Aac, Some(vec![0xF9, 0x40, 0, 0]))), "USAC");
        assert!(!stream_ok(&with(StreamKind::Audio, Codec::Alac, None)), "ALAC needs its cookie");
        assert!(stream_ok(&with(StreamKind::Audio, Codec::Ac3, None)));
    }
}
