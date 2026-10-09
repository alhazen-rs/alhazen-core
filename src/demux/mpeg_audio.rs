//! MPEG audio (Layer III) and ADTS frame headers, shared by detection and the MP3/ADTS readers.

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

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
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

#[cfg(feature = "native")]
use crate::Result;
#[cfg(feature = "native")]
use crate::demux::Metadata;
#[cfg(feature = "native")]
use crate::demux::window::ReadWindow;

/// Bytes searched per step when resynchronising, and the overlap between steps (an ADTS frame can
/// be up to 8 KiB, so two chained frames fit in the overlap).
#[cfg(feature = "native")]
const RESYNC_WINDOW: usize = 64 * 1024;
#[cfg(feature = "native")]
const RESYNC_OVERLAP: usize = 16 * 1024;
/// ID3v2 tags larger than this are skipped unread.
#[cfg(feature = "native")]
const MAX_TAG: u64 = 64 << 20;

/// Chained MPEG audio / ADTS frames in `[first, end)`: reading, resync over junk, and an index of
/// frame offsets for exact seeking (local files).
#[cfg(feature = "native")]
pub(crate) struct Frames<H: FrameHeader> {
    pub w: ReadWindow,
    pub first: u64,
    pub end: u64,
    pos: u64,
    /// Number of the frame at `pos`.
    index: u64,
    /// Offsets of frames 0, 1, …, valid while `exact`.
    offsets: Vec<u64>,
    exact: bool,
    like: H,
}

#[cfg(feature = "native")]
impl<H: FrameHeader> Frames<H> {
    pub fn new(w: ReadWindow, first: u64, end: u64, like: H) -> Self {
        Self { w, first, end, pos: first, index: 0, offsets: Vec::new(), exact: true, like }
    }

    /// A header of this stream at `at` whose frame ends within the audio.
    fn header_at(&mut self, at: u64) -> Result<Option<H>> {
        let b = self.w.at(at, 16)?;
        Ok(H::read(b).filter(|h| self.like.same_stream(h) && at + h.length() as u64 <= self.end))
    }

    /// The first position at or after `from` where two frames of this stream chain.
    fn find_frame(&mut self, from: u64) -> Result<Option<u64>> {
        let mut at = from;
        while at < self.end {
            let window = self.w.at(at, RESYNC_WINDOW)?.to_vec();
            if let Some((i, _)) = find_chain(&window, 2, Some(&self.like)) {
                return Ok(Some(at + i as u64));
            }
            if window.len() < RESYNC_WINDOW {
                return Ok(None);
            }
            at += (RESYNC_WINDOW - RESYNC_OVERLAP) as u64;
        }
        Ok(None)
    }

    /// The next frame: (frame number, header, the whole frame). Junk between frames is skipped.
    pub fn next(&mut self) -> Result<Option<(u64, H, Vec<u8>)>> {
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            let Some(h) = self.header_at(self.pos)? else {
                match self.find_frame(self.pos + 1)? {
                    Some(at) => self.pos = at,
                    None => self.pos = self.end,
                }
                continue;
            };
            let bytes = self.w.at(self.pos, h.length())?.to_vec();
            if bytes.len() < h.length() {
                return Ok(None); // truncated last frame
            }
            if self.exact && self.index == self.offsets.len() as u64 {
                self.offsets.push(self.pos);
            }
            let n = self.index;
            self.index += 1;
            self.pos += h.length() as u64;
            return Ok(Some((n, h, bytes)));
        }
    }

    /// Exact seek to frame `n` (or the last frame), extending the offset index by reading frame
    /// headers only. Returns the frame number reached.
    pub fn seek_exact(&mut self, n: u64) -> Result<u64> {
        if !self.exact || self.offsets.is_empty() {
            self.offsets = vec![self.first];
            self.exact = true;
        }
        while (self.offsets.len() as u64) <= n {
            let at = *self.offsets.last().expect("not empty");
            let Some(h) = self.header_at(at)? else { break };
            let next = at + h.length() as u64;
            let next = if self.header_at(next)?.is_some() {
                next
            } else {
                match self.find_frame(next)? {
                    Some(p) => p,
                    None => break,
                }
            };
            self.offsets.push(next);
        }
        let k = n.min(self.offsets.len() as u64 - 1);
        self.pos = self.offsets[k as usize];
        self.index = k;
        Ok(k)
    }

    /// Approximate seek without scanning: to the first frame at or after byte `offset`, taken to
    /// be frame `n`.
    pub fn seek_approx(&mut self, offset: u64, n: u64) -> Result<u64> {
        let offset = offset.clamp(self.first, self.end);
        self.pos = match self.header_at(offset)? {
            Some(_) => offset,
            None => self.find_frame(offset)?.unwrap_or(self.end),
        };
        self.index = n;
        self.exact = false;
        Ok(n)
    }

    /// The number of frames, by scanning every header (local files). The read position is kept.
    pub fn count(&mut self) -> Result<u64> {
        let (pos, index) = (self.pos, self.index);
        self.seek_exact(u64::MAX)?;
        let n = self.offsets.len() as u64;
        (self.pos, self.index) = (pos, index);
        Ok(n)
    }
}

/// Reads the ID3v2 tags at the start of the file into `meta`; returns the offset after them.
#[cfg(feature = "native")]
pub(crate) fn read_id3v2_tags(w: &mut ReadWindow, meta: &mut Metadata) -> Result<u64> {
    let mut start = 0u64;
    for _ in 0..4 {
        let head = w.at(start, 10)?.to_vec();
        let Some(len) = crate::demux::tags::id3::id3v2_len(&head) else { break };
        if len <= MAX_TAG {
            let tag = w.at(start, len as usize)?.to_vec();
            crate::demux::tags::id3::parse_id3v2(&tag, meta);
        }
        start += len;
    }
    Ok(start)
}

/// For local files: reads an ID3v1 trailer into `meta` and returns where the audio ends (before it).
#[cfg(feature = "native")]
pub(crate) fn read_id3v1(w: &mut ReadWindow, meta: &mut Metadata) -> Result<Option<u64>> {
    let Some(len) = w.len().filter(|&l| l >= 128 && w.is_local()) else { return Ok(None) };
    let trailer = w.at(len - 128, 128)?.to_vec();
    if !trailer.starts_with(b"TAG") {
        return Ok(None);
    }
    crate::demux::tags::id3::parse_id3v1(&trailer, meta);
    Ok(Some(len - 128))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
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

    #[cfg(feature = "native")]
    #[test]
    fn frames_read_resync_and_seek_exactly() {
        use crate::demux::window::ReadWindow;
        use crate::source::FileSource;
        let mut bytes = vec![0u8; 50];
        let mut offsets = Vec::new();
        for i in 0..20 {
            if i == 10 {
                bytes.extend([0x12u8; 300]); // junk in the middle
            }
            offsets.push(bytes.len() as u64);
            let start = bytes.len();
            bytes.extend([0xFF, 0xFB, 0x90, 0x64, i as u8]);
            bytes.resize(start + 417, 0);
        }
        let path = std::env::temp_dir().join(format!("frames_{}.bin", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let w = ReadWindow::new(Box::new(FileSource::open(&path).unwrap()));
        let like = MpegHeader::parse(&bytes[50..]).unwrap();
        let mut f = Frames::new(w, 50, bytes.len() as u64, like);
        let mut seen = Vec::new();
        while let Some((n, _, data)) = f.next().unwrap() {
            seen.push((n, data[4]));
        }
        assert_eq!(seen.len(), 20, "junk skipped, every frame read");
        assert!(seen.iter().enumerate().all(|(i, &(n, tag))| n == i as u64 && tag == i as u8));
        assert_eq!(f.seek_exact(15).unwrap(), 15);
        assert_eq!(f.next().unwrap().unwrap().2[4], 15);
        assert_eq!(f.count().unwrap(), 20);
        f.seek_approx(offsets[12] - 100, 12).unwrap();
        assert_eq!(f.next().unwrap().unwrap().2[4], 12, "approximate seek resyncs to the next frame");
    }
}
