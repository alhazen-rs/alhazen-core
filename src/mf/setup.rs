//! Codec setup data in the forms Media Foundation's audio decoders expect (`MF_MT_USER_DATA`).

use crate::demux::Codec;

/// `MF_MT_USER_DATA` for an audio stream: for AAC the part of `HEAACWAVEINFO` after its
/// `WAVEFORMATEX` (payload type raw, profile/level unknown) followed by the
/// AudioSpecificConfig; for ALAC the ALACSpecificConfig ("magic cookie"). `None` when the codec needs none or data is missing.
pub fn audio_user_data(codec: &Codec, extradata: Option<&[u8]>) -> Option<Vec<u8>> {
    match codec {
        Codec::Aac => {
            let asc = extradata?;
            // wPayloadType 0 (raw), wAudioProfileLevelIndication 0xFE (unknown),
            // wStructType 0, wReserved1 0, dwReserved2 0.
            let mut v = vec![0, 0, 0xFE, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            v.extend_from_slice(asc);
            Some(v)
        }
        Codec::Alac => extradata.filter(|c| c.len() >= 24).map(|c| c[..24].to_vec()),
        _ => None,
    }
}

/// Bits per sample of an ALAC stream (from its cookie), which Windows' ALAC decoder requires on
/// its input type.
pub fn audio_bits(codec: &Codec, extradata: Option<&[u8]>) -> Option<u32> {
    let data = audio_user_data(codec, extradata)?;
    match codec {
        // ALACSpecificConfig: frameLength u32, compatibleVersion u8, bitDepth u8, …
        Codec::Alac => data.get(5).map(|&b| b as u32),
        _ => None,
    }
}

/// Duration of one coded frame when the codec's frames have a fixed length (ALAC: its cookie's
/// frameLength); Windows' ALAC decoder rejects input samples without a duration.
pub fn frame_duration(codec: &Codec, extradata: Option<&[u8]>, rate: u32) -> Option<std::time::Duration> {
    match codec {
        Codec::Alac if rate > 0 => {
            let cookie = audio_user_data(codec, extradata)?;
            let frames = u32::from_be_bytes(cookie.get(..4)?.try_into().ok()?);
            Some(std::time::Duration::from_secs_f64(frames as f64 / rate as f64))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_user_data_is_heaacwaveinfo_tail_plus_asc() {
        let v = audio_user_data(&Codec::Aac, Some(&[0x12, 0x10])).unwrap();
        assert_eq!(v.len(), 12 + 2);
        assert_eq!(&v[..4], &[0, 0, 0xFE, 0], "raw payload, unknown profile");
        assert_eq!(&v[12..], &[0x12, 0x10]);
        assert!(audio_user_data(&Codec::Aac, None).is_none());
    }

    #[test]
    fn alac_bit_depth_and_frame_duration() {
        let mut cookie = [0u8; 24];
        cookie[..4].copy_from_slice(&4096u32.to_be_bytes());
        cookie[5] = 16;
        assert_eq!(audio_bits(&Codec::Alac, Some(&cookie)), Some(16));
        assert_eq!(frame_duration(&Codec::Alac, Some(&cookie), 48_000), Some(std::time::Duration::from_secs_f64(4096.0 / 48_000.0)));
        assert_eq!(frame_duration(&Codec::Aac, Some(&cookie), 48_000), None);
    }

    #[cfg(feature = "native")]
    #[test]
    fn reads_the_fixtures_setup_data() {
        use crate::demux::{Demuxer, Mp4Demuxer};
        use crate::source::FileSource;
        let alac = Mp4Demuxer::open(Box::new(FileSource::open("tests/fixtures/alac.m4a").unwrap())).unwrap();
        let s = &alac.streams()[0];
        assert_eq!(audio_bits(&Codec::Alac, s.extradata.as_deref()), Some(16));
        assert!(frame_duration(&Codec::Alac, s.extradata.as_deref(), s.sample_rate).is_some());
    }

    #[test]
    fn alac_user_data_is_the_24_byte_cookie() {
        assert_eq!(audio_user_data(&Codec::Alac, Some(&[1; 30])).unwrap(), [1; 24]);
        assert!(audio_user_data(&Codec::Alac, Some(&[1; 10])).is_none());
        assert!(audio_user_data(&Codec::Mp3, Some(&[1])).is_none());
    }
}
