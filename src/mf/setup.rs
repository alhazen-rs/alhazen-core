//! Codec setup data in the forms Media Foundation's audio decoders expect (`MF_MT_USER_DATA`).

use crate::demux::Codec;

/// `MF_MT_USER_DATA` for an audio stream: for AAC the part of `HEAACWAVEINFO` after its
/// `WAVEFORMATEX` (payload type raw, profile/level unknown) followed by the
/// AudioSpecificConfig; for FLAC the `fLaC` marker and STREAMINFO block; for ALAC the
/// ALACSpecificConfig ("magic cookie"). `None` when the codec needs none or data is missing.
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
        Codec::Flac => flac_streaminfo(extradata?),
        Codec::Alac => extradata.filter(|c| c.len() >= 24).map(|c| c[..24].to_vec()),
        _ => None,
    }
}

/// Bits per sample of a lossless stream (FLAC STREAMINFO, ALAC cookie), which Windows' FLAC and
/// ALAC decoders require on their input type.
pub fn audio_bits(codec: &Codec, extradata: Option<&[u8]>) -> Option<u32> {
    let data = audio_user_data(codec, extradata)?;
    match codec {
        // fLaC(4) + block header(4) + STREAMINFO: bits-per-sample − 1 is 5 bits at byte 12 bit 0 ..
        // byte 13 bit 4.
        Codec::Flac => {
            let info = data.get(8..)?;
            Some((((info.get(12)? & 1) << 4 | info.get(13)? >> 4) + 1) as u32)
        }
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

/// `fLaC` + the STREAMINFO metadata block (header + 34 bytes) from a Matroska/MP4 FLAC header.
fn flac_streaminfo(header: &[u8]) -> Option<Vec<u8>> {
    let blocks = header.strip_prefix(b"fLaC")?;
    // Metadata block header: last-flag + type (7 bits), 24-bit length. STREAMINFO is type 0, first.
    let (kind, len) = (blocks.first()? & 0x7F, u32::from_be_bytes([0, *blocks.get(1)?, *blocks.get(2)?, *blocks.get(3)?]));
    if kind != 0 || len != 34 {
        return None;
    }
    let mut v = b"fLaC".to_vec();
    v.push(0x80); // last metadata block
    v.extend_from_slice(&blocks[1..4 + 34]);
    Some(v)
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
    fn flac_user_data_is_marker_and_streaminfo() {
        // fLaC, STREAMINFO (not last, 34 bytes of 7s), then a PADDING block.
        let mut h = b"fLaC".to_vec();
        h.extend_from_slice(&[0x00, 0, 0, 34]);
        h.extend_from_slice(&[7; 34]);
        h.extend_from_slice(&[0x81, 0, 0, 2, 0, 0]);
        let v = audio_user_data(&Codec::Flac, Some(&h)).unwrap();
        assert_eq!(v.len(), 4 + 4 + 34);
        assert_eq!(&v[..5], b"fLaC\x80");
        assert!(v[8..].iter().all(|&b| b == 7));
        assert!(audio_user_data(&Codec::Flac, Some(b"junk")).is_none());
    }

    #[test]
    fn lossless_bit_depth_and_alac_frame_duration() {
        // STREAMINFO with 24 bits per sample: byte 12 bit 0 = 1, byte 13 high nibble = 0x7 (23).
        let mut h = b"fLaC".to_vec();
        h.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut info = [0u8; 34];
        info[12] = 0x01;
        info[13] = 0x70;
        h.extend_from_slice(&info);
        assert_eq!(audio_bits(&Codec::Flac, Some(&h)), Some(24));
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
        use crate::demux::{Demuxer, MatroskaDemuxer, Mp4Demuxer};
        use crate::source::FileSource;
        let flac = MatroskaDemuxer::open(Box::new(FileSource::open("tests/fixtures/flac.mkv").unwrap())).unwrap();
        assert_eq!(audio_bits(&Codec::Flac, flac.streams()[0].extradata.as_deref()), Some(16));
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
