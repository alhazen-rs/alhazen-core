//! Media Foundation identifiers for our codecs.

use windows::Win32::Media::MediaFoundation::*;
use windows::core::GUID;

use crate::hw::select::HwCodec;

/// Output subtypes we accept, in order of preference.
pub fn outputs(codec: HwCodec) -> &'static [GUID] {
    match codec {
        HwCodec::H264 | HwCodec::Hevc | HwCodec::Vp8 | HwCodec::Vp9 | HwCodec::Av1 | HwCodec::ProRes => &[MFVideoFormat_NV12, MFVideoFormat_P010],
        _ => &[MFAudioFormat_Float, MFAudioFormat_PCM],
    }
}

/// (transform category, major type, input subtype).
pub fn ids(codec: HwCodec) -> (GUID, GUID, GUID) {
    let video = |s| (MFT_CATEGORY_VIDEO_DECODER, MFMediaType_Video, s);
    let audio = |s| (MFT_CATEGORY_AUDIO_DECODER, MFMediaType_Audio, s);
    match codec {
        HwCodec::H264 => video(MFVideoFormat_H264),
        HwCodec::Hevc => video(MFVideoFormat_HEVC),
        HwCodec::Vp8 => video(MFVideoFormat_VP80),
        HwCodec::Vp9 => video(MFVideoFormat_VP90),
        HwCodec::Av1 => video(MFVideoFormat_AV1),
        // Never asked: the catalogue refuses ProRes before looking for a decoder.
        HwCodec::ProRes => video(GUID::zeroed()),
        HwCodec::Aac => audio(MFAudioFormat_AAC),
        HwCodec::Mp3 => audio(MFAudioFormat_MP3),
        HwCodec::Ac3 => audio(MFAudioFormat_Dolby_AC3),
        HwCodec::Eac3 => audio(MFAudioFormat_Dolby_DDPlus),
        HwCodec::Alac => audio(MFAudioFormat_ALAC),
    }
}
