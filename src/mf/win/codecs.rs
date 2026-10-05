//! Media Foundation identifiers for our codecs.

use windows::Win32::Media::MediaFoundation::*;
use windows::core::GUID;

use super::super::select::MfCodec;

/// (transform category, major type, input subtype).
pub fn ids(codec: MfCodec) -> (GUID, GUID, GUID) {
    let video = |s| (MFT_CATEGORY_VIDEO_DECODER, MFMediaType_Video, s);
    let audio = |s| (MFT_CATEGORY_AUDIO_DECODER, MFMediaType_Audio, s);
    match codec {
        MfCodec::H264 => video(MFVideoFormat_H264),
        MfCodec::Hevc => video(MFVideoFormat_HEVC),
        MfCodec::Vp9 => video(MFVideoFormat_VP90),
        MfCodec::Av1 => video(MFVideoFormat_AV1),
        MfCodec::Aac => audio(MFAudioFormat_AAC),
        MfCodec::Mp3 => audio(MFAudioFormat_MP3),
        MfCodec::Ac3 => audio(MFAudioFormat_Dolby_AC3),
        MfCodec::Eac3 => audio(MFAudioFormat_Dolby_DDPlus),
        MfCodec::Flac => audio(MFAudioFormat_FLAC),
        MfCodec::Alac => audio(MFAudioFormat_ALAC),
    }
}
