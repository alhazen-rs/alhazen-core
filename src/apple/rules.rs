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
}
