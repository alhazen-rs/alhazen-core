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
    fn alac_user_data_is_the_24_byte_cookie() {
        assert_eq!(audio_user_data(&Codec::Alac, Some(&[1; 30])).unwrap(), [1; 24]);
        assert!(audio_user_data(&Codec::Alac, Some(&[1; 10])).is_none());
        assert!(audio_user_data(&Codec::Mp3, Some(&[1])).is_none());
    }
}
