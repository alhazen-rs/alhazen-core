//! H.264/HEVC packaging: length-prefixed NAL units (MP4 `avcC`/`hvcC`, Matroska) to Annex B
//! start-code streams, as hardware decoders such as Media Foundation's expect.

use crate::{Error, Result};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Which configuration record the codec setup data is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamSetFormat {
    /// H.264 `AVCDecoderConfigurationRecord`.
    Avcc,
    /// HEVC `HEVCDecoderConfigurationRecord`.
    Hvcc,
}

/// Converts one stream's packets to Annex B.
#[derive(Clone, Debug)]
pub struct AnnexB {
    /// Bytes in each NAL unit's length prefix (1, 2 or 4).
    length_size: usize,
    /// The parameter sets (SPS/PPS, plus VPS for HEVC) as Annex B, sent before every keyframe
    /// so decoding can start at any of them (seeks).
    header: Vec<u8>,
}

impl AnnexB {
    /// From the configuration record (`StreamInfo::extradata`); `None` if it is malformed.
    pub fn from_config(format: ParamSetFormat, config: &[u8]) -> Option<AnnexB> {
        let (length_size, sets) = match format {
            ParamSetFormat::Avcc => parse_avcc(config)?,
            ParamSetFormat::Hvcc => parse_hvcc(config)?,
        };
        if !matches!(length_size, 1 | 2 | 4) {
            return None;
        }
        let mut header = Vec::new();
        for set in sets {
            header.extend_from_slice(&START_CODE);
            header.extend_from_slice(set);
        }
        Some(AnnexB { length_size, header })
    }

    /// Appends `packet` as Annex B to `out`, preceded by the parameter sets for keyframes.
    pub fn convert(&self, packet: &[u8], keyframe: bool, out: &mut Vec<u8>) -> Result<()> {
        if keyframe {
            out.extend_from_slice(&self.header);
        }
        let mut pos = 0;
        while pos < packet.len() {
            let len_bytes = packet
                .get(pos..pos + self.length_size)
                .ok_or_else(|| Error::Decode("NAL length prefix truncated".into()))?;
            let len = len_bytes.iter().fold(0usize, |n, &b| n << 8 | b as usize);
            pos += self.length_size;
            let nal = packet.get(pos..pos + len).ok_or_else(|| Error::Decode("NAL unit truncated".into()))?;
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
            pos += len;
        }
        Ok(())
    }
}

/// `AVCDecoderConfigurationRecord`: length size and SPS/PPS NAL units.
fn parse_avcc(c: &[u8]) -> Option<(usize, Vec<&[u8]>)> {
    if *c.first()? != 1 {
        return None;
    }
    let length_size = (*c.get(4)? & 3) as usize + 1;
    let mut sets = Vec::new();
    let mut pos = 5;
    for mask in [0x1F, 0xFF] {
        // numOfSequenceParameterSets (low 5 bits), then numOfPictureParameterSets.
        let count = (*c.get(pos)? & mask) as usize;
        pos += 1;
        for _ in 0..count {
            let len = u16::from_be_bytes(c.get(pos..pos + 2)?.try_into().ok()?) as usize;
            sets.push(c.get(pos + 2..pos + 2 + len)?);
            pos += 2 + len;
        }
    }
    Some((length_size, sets))
}

/// `HEVCDecoderConfigurationRecord`: length size and VPS/SPS/PPS (and SEI) NAL units.
fn parse_hvcc(c: &[u8]) -> Option<(usize, Vec<&[u8]>)> {
    let length_size = (*c.get(21)? & 3) as usize + 1;
    let arrays = *c.get(22)? as usize;
    let mut sets = Vec::new();
    let mut pos = 23;
    for _ in 0..arrays {
        let count = u16::from_be_bytes(c.get(pos + 1..pos + 3)?.try_into().ok()?) as usize;
        pos += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes(c.get(pos..pos + 2)?.try_into().ok()?) as usize;
            sets.push(c.get(pos + 2..pos + 2 + len)?);
            pos += 2 + len;
        }
    }
    Some((length_size, sets))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// avcC with one SPS [0x67, 1, 2] and one PPS [0x68, 3], `length_size` byte prefixes.
    fn avcc(length_size: u8) -> Vec<u8> {
        vec![1, 0x64, 0, 0x1F, 0xFC | (length_size - 1), 0xE1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 3]
    }

    #[test]
    fn keyframes_get_parameter_sets_and_every_nal_a_start_code() {
        for size in [1u8, 2, 4] {
            let conv = AnnexB::from_config(ParamSetFormat::Avcc, &avcc(size)).unwrap();
            let mut pkt = Vec::new();
            for nal in [&[0x65u8, 9, 9][..], &[0x06, 7]] {
                let len = nal.len() as u32;
                pkt.extend_from_slice(&len.to_be_bytes()[4 - size as usize..]);
                pkt.extend_from_slice(nal);
            }
            let mut out = Vec::new();
            conv.convert(&pkt, true, &mut out).unwrap();
            assert_eq!(out, [&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3][..], &[0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0, 1, 6, 7]].concat(), "size {size}");
            out.clear();
            conv.convert(&pkt, false, &mut out).unwrap();
            assert_eq!(out, [0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0, 1, 6, 7], "non-keyframes carry no parameter sets");
        }
    }

    #[test]
    fn truncated_packets_and_configs_are_errors() {
        let conv = AnnexB::from_config(ParamSetFormat::Avcc, &avcc(4)).unwrap();
        let mut out = Vec::new();
        assert!(conv.convert(&[0, 0, 0, 9, 1, 2], false, &mut out).is_err(), "length past the end");
        assert!(conv.convert(&[0, 0], false, &mut out).is_err(), "prefix cut short");
        assert!(AnnexB::from_config(ParamSetFormat::Avcc, &avcc(4)[..9]).is_none());
        assert!(AnnexB::from_config(ParamSetFormat::Avcc, &[0; 4]).is_none());
        assert!(AnnexB::from_config(ParamSetFormat::Hvcc, &[1; 10]).is_none());
    }
}
