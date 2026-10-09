//! MPEG audio (Layer III) and ADTS frame headers, shared by detection and the MP3/ADTS readers.

// Several fields and helpers serve only the MP3/ADTS readers (Tasks 7–8).
#![allow(dead_code)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MpegVersion {
    V1,
    V2,
    V25,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MpegHeader {
    pub version: MpegVersion,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u16,
    /// The whole frame in bytes, header included.
    pub frame_len: usize,
    /// PCM frames per MPEG frame: 1152 (MPEG-1) or 576.
    pub samples: u32,
}

const BITRATES_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const BITRATES_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

impl MpegHeader {
    /// A Layer III header at the start of `b`. Free-format bitrates are not supported.
    pub fn parse(b: &[u8]) -> Option<Self> {
        let b = b.get(..4)?;
        if b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
            return None;
        }
        let version = match (b[1] >> 3) & 3 {
            0 => MpegVersion::V25,
            2 => MpegVersion::V2,
            3 => MpegVersion::V1,
            _ => return None,
        };
        if (b[1] >> 1) & 3 != 1 {
            return None; // Layer III only
        }
        let index = (b[2] >> 4) as usize;
        let rate_index = ((b[2] >> 2) & 3) as usize;
        if index == 0 || index == 15 || rate_index == 3 {
            return None;
        }
        let base = [44_100, 48_000, 32_000][rate_index];
        let (sample_rate, bitrate_kbps, samples) = match version {
            MpegVersion::V1 => (base, BITRATES_V1[index], 1152),
            MpegVersion::V2 => (base / 2, BITRATES_V2[index], 576),
            MpegVersion::V25 => (base / 4, BITRATES_V2[index], 576),
        };
        let padding = ((b[2] >> 1) & 1) as usize;
        let channels = if b[3] >> 6 == 3 { 1 } else { 2 };
        let frame_len = (samples as usize / 8) * bitrate_kbps as usize * 1000 / sample_rate as usize + padding;
        Some(Self { version, bitrate_kbps, sample_rate, channels, frame_len, samples })
    }

    /// Bytes of side information after the 4-byte header; a Xing/Info header starts after them.
    pub fn side_info_len(&self) -> usize {
        match (self.version, self.channels) {
            (MpegVersion::V1, 1) => 17,
            (MpegVersion::V1, _) => 32,
            (_, 1) => 9,
            _ => 17,
        }
    }
}

/// AAC sampling frequencies by index (ISO 14496-3 Table 1.18).
pub(crate) const AAC_RATES: [u32; 13] =
    [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdtsHeader {
    /// Audio object type − 1 (1 = AAC-LC).
    pub profile: u8,
    pub rate_index: u8,
    pub sample_rate: u32,
    pub channel_config: u8,
    /// 7 bytes, or 9 with a CRC.
    pub header_len: usize,
    /// The whole frame in bytes, header included.
    pub frame_len: usize,
    /// Raw data blocks in the frame (1024 samples each).
    pub blocks: u32,
}

impl AdtsHeader {
    pub fn parse(b: &[u8]) -> Option<Self> {
        let b = b.get(..7)?;
        if b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
            return None;
        }
        let rate_index = (b[2] >> 2) & 0xF;
        let sample_rate = *AAC_RATES.get(rate_index as usize)?;
        let channel_config = ((b[2] & 1) << 2) | (b[3] >> 6);
        let header_len = if b[1] & 1 == 1 { 7 } else { 9 };
        let frame_len = (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | ((b[5] as usize) >> 5);
        if frame_len <= header_len {
            return None;
        }
        Some(Self { profile: b[2] >> 6, rate_index, sample_rate, channel_config, header_len, frame_len, blocks: (b[6] & 3) as u32 + 1 })
    }

    /// The 2-byte AudioSpecificConfig this header describes.
    pub fn audio_specific_config(&self) -> Vec<u8> {
        let aot = self.profile as u16 + 1;
        (aot << 11 | (self.rate_index as u16) << 7 | (self.channel_config as u16) << 3).to_be_bytes().to_vec()
    }
}

/// A frame header whose frames are stored back to back.
pub(crate) trait FrameHeader: Copy {
    fn read(b: &[u8]) -> Option<Self>;
    fn length(&self) -> usize;
    /// Whether `other` belongs to the same stream (same version/profile, rate and layout).
    fn same_stream(&self, other: &Self) -> bool;
    fn frame_samples(&self) -> u32;
    fn rate(&self) -> u32;
}

impl FrameHeader for MpegHeader {
    fn read(b: &[u8]) -> Option<Self> {
        Self::parse(b)
    }
    fn length(&self) -> usize {
        self.frame_len
    }
    fn same_stream(&self, other: &Self) -> bool {
        self.version == other.version && self.sample_rate == other.sample_rate
    }
    fn frame_samples(&self) -> u32 {
        self.samples
    }
    fn rate(&self) -> u32 {
        self.sample_rate
    }
}

impl FrameHeader for AdtsHeader {
    fn read(b: &[u8]) -> Option<Self> {
        Self::parse(b)
    }
    fn length(&self) -> usize {
        self.frame_len
    }
    fn same_stream(&self, other: &Self) -> bool {
        self.profile == other.profile && self.rate_index == other.rate_index && self.channel_config == other.channel_config
    }
    fn frame_samples(&self) -> u32 {
        1024 * self.blocks
    }
    fn rate(&self) -> u32 {
        self.sample_rate
    }
}

/// `count` frames chained at `at` in `buf`, each starting where the previous one ends, all from one
/// stream (and from the same stream as `like`, when given). Returns the first header.
pub(crate) fn chain<H: FrameHeader>(buf: &[u8], at: usize, count: usize, like: Option<&H>) -> Option<H> {
    let first = H::read(buf.get(at..)?)?;
    if like.is_some_and(|l| !l.same_stream(&first)) {
        return None;
    }
    let mut pos = at + first.length();
    for _ in 1..count {
        let next = H::read(buf.get(pos..)?)?;
        if !first.same_stream(&next) {
            return None;
        }
        pos += next.length();
    }
    Some(first)
}

/// The first position in `buf` where `count` frames chain.
pub(crate) fn find_chain<H: FrameHeader>(buf: &[u8], count: usize, like: Option<&H>) -> Option<(usize, H)> {
    (0..buf.len()).find_map(|i| chain(buf, i, count, like).map(|h| (i, h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_mpeg1_layer3_header() {
        // MPEG-1 Layer III, 128 kb/s, 44.1 kHz, no padding, joint stereo.
        let h = MpegHeader::parse(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        assert_eq!((h.version, h.bitrate_kbps, h.sample_rate, h.channels), (MpegVersion::V1, 128, 44_100, 2));
        assert_eq!((h.frame_len, h.samples, h.side_info_len()), (417, 1152, 32));
        // MPEG-2.5 (version bits 00), 8 kHz index 2 (= 32000/4), 16 kb/s, mono.
        let h = MpegHeader::parse(&[0xFF, 0xE3, 0x28, 0xC0]).unwrap();
        assert_eq!((h.version, h.sample_rate, h.bitrate_kbps, h.channels), (MpegVersion::V25, 8_000, 16, 1));
        assert_eq!(h.frame_len, 72 * 16_000 / 8_000);
    }

    #[test]
    fn rejects_other_layers_and_bad_fields() {
        assert!(MpegHeader::parse(&[0xFF, 0xFD, 0x90, 0x64]).is_none(), "Layer II");
        assert!(MpegHeader::parse(&[0xFF, 0xFB, 0xF0, 0x64]).is_none(), "bitrate index 15");
        assert!(MpegHeader::parse(&[0xFF, 0xFB, 0x9C, 0x64]).is_none(), "rate index 3");
        assert!(MpegHeader::parse(&[0xFF, 0xF1, 0x50, 0x80]).is_none(), "ADTS is not MP3");
    }

    #[test]
    fn parses_an_adts_header_and_builds_its_config() {
        // AAC-LC, 44.1 kHz (index 4), stereo, 371-byte frame, no CRC.
        let h = AdtsHeader::parse(&[0xFF, 0xF1, 0x50, 0x80, 0x2E, 0x7F, 0xFC]).unwrap();
        assert_eq!((h.profile, h.sample_rate, h.channel_config), (1, 44_100, 2));
        assert_eq!((h.header_len, h.frame_len, h.blocks), (7, 371, 1));
        assert_eq!(h.audio_specific_config(), [0x12, 0x10]);
    }

    #[test]
    fn finds_a_chain_of_frames_after_junk() {
        let frame = |b: &mut Vec<u8>| {
            let start = b.len();
            b.extend([0xFF, 0xFB, 0x90, 0x64]);
            b.resize(start + 417, 0);
        };
        let mut buf = vec![0u8; 100];
        for _ in 0..3 {
            frame(&mut buf);
        }
        let (at, h) = find_chain::<MpegHeader>(&buf, 3, None).unwrap();
        assert_eq!((at, h.frame_len), (100, 417));
        assert!(find_chain::<MpegHeader>(&buf, 4, None).is_none(), "only three frames");
    }
}
