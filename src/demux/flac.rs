//! Native FLAC (`fLaC`): metadata blocks, frames found by sync code and CRCs, seeking by SEEKTABLE
//! or bisection.

use std::time::Duration;

use super::metadata::{CoverPick, MAX_PICTURE};
use super::window::ReadWindow;
use super::{Codec, Demuxer, Metadata, Packet, StreamInfo, StreamKind, tags};
use crate::source::MediaSource;
use crate::{Error, Result};

/// Frames longer than this mean the data is not FLAC.
const MAX_FRAME: usize = 16 << 20;
const CHUNK: usize = 64 * 1024;

/// The STREAMINFO fields readers need.
pub(crate) struct FlacInfo {
    pub rate: u32,
    pub channels: u16,
    pub min_block: u32,
    /// Total samples per channel; 0 when unknown.
    pub total: u64,
}

/// A STREAMINFO block body (34 bytes).
pub(crate) fn parse_streaminfo(b: &[u8]) -> Option<FlacInfo> {
    let b = b.get(..34)?;
    let rate = (b[10] as u32) << 12 | (b[11] as u32) << 4 | (b[12] as u32) >> 4;
    let info = FlacInfo {
        rate,
        channels: ((b[12] >> 1) & 7) as u16 + 1,
        min_block: u16::from_be_bytes([b[0], b[1]]) as u32,
        total: ((b[13] & 0xF) as u64) << 32 | u32::from_be_bytes([b[14], b[15], b[16], b[17]]) as u64,
    };
    (rate > 0).then_some(info)
}

/// The decoder's setup bytes: `fLaC` and STREAMINFO as the last metadata block.
pub(crate) fn flac_extradata(streaminfo: &[u8]) -> Vec<u8> {
    let mut e = b"fLaC\x80\x00\x00\x22".to_vec();
    e.extend(&streaminfo[..34.min(streaminfo.len())]);
    e
}

pub(crate) struct FlacFrame {
    /// First sample of the frame.
    pub sample: u64,
    pub block_size: u32,
    pub header_len: usize,
}

/// A frame header at the start of `b`, checked by its CRC-8 and against STREAMINFO.
pub(crate) fn flac_frame_header(b: &[u8], info: &FlacInfo) -> Option<FlacFrame> {
    if b.len() < 6 || b[0] != 0xFF || b[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable = b[1] & 1 == 1;
    let (size_code, rate_code, channel_code, depth_code) = (b[2] >> 4, b[2] & 0xF, b[3] >> 4, (b[3] >> 1) & 7);
    if size_code == 0 || rate_code == 0xF || channel_code > 10 || depth_code == 3 || b[3] & 1 != 0 {
        return None;
    }
    let channels = if channel_code < 8 { channel_code as u16 + 1 } else { 2 };
    if channels != info.channels {
        return None;
    }
    let (number, n) = utf8_number(&b[4..])?;
    let mut pos = 4 + n;
    let block_size = match size_code {
        1 => 192,
        2..=5 => 576 << (size_code - 2),
        6 => {
            pos += 1;
            *b.get(pos - 1)? as u32 + 1
        }
        7 => {
            pos += 2;
            u16::from_be_bytes([*b.get(pos - 2)?, *b.get(pos - 1)?]) as u32 + 1
        }
        _ => 256 << (size_code - 8),
    };
    pos += match rate_code {
        12 => 1,
        13 | 14 => 2,
        _ => 0,
    };
    if crc8(b.get(..pos)?) != *b.get(pos)? {
        return None;
    }
    let sample = if variable { number } else { number * info.min_block.max(1) as u64 };
    Some(FlacFrame { sample, block_size, header_len: pos + 1 })
}

/// FLAC's UTF-8-style coded frame/sample number: (value, bytes used).
fn utf8_number(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let ones = first.leading_ones() as usize;
    if ones == 1 || ones > 7 {
        return None;
    }
    let len = ones.max(1);
    let mut v = if ones == 0 { first as u64 } else { (first & (0x7F >> ones)) as u64 };
    for i in 1..len {
        let c = *b.get(i)?;
        if c & 0xC0 != 0x80 {
            return None;
        }
        v = v << 6 | (c & 0x3F) as u64;
    }
    Some((v, len))
}

fn crc8(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |mut c, &x| {
        c ^= x;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { c << 1 ^ 0x07 } else { c << 1 };
        }
        c
    })
}

fn crc16_update(mut c: u16, x: u8) -> u16 {
    c ^= (x as u16) << 8;
    for _ in 0..8 {
        c = if c & 0x8000 != 0 { c << 1 ^ 0x8005 } else { c << 1 };
    }
    c
}

/// SEEKTABLE points (first sample, byte offset from the first frame), placeholders skipped.
fn parse_seektable(b: &[u8]) -> Vec<(u64, u64)> {
    b.as_chunks::<18>()
        .0
        .iter()
        .map(|p| (u64::from_be_bytes(p[..8].try_into().unwrap()), u64::from_be_bytes(p[8..16].try_into().unwrap())))
        .filter(|&(sample, _)| sample != u64::MAX)
        .collect()
}

pub struct FlacDemuxer {
    w: ReadWindow,
    streams: Vec<StreamInfo>,
    metadata: Option<Metadata>,
    info: FlacInfo,
    seektable: Vec<(u64, u64)>,
    first_frame: u64,
    end: u64,
    pos: u64,
}

impl FlacDemuxer {
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut w = ReadWindow::new(src);
        if w.at(0, 4)? != b"fLaC" {
            return Err(Error::UnsupportedContainer);
        }
        let (mut meta, mut covers) = (Metadata::default(), CoverPick::default());
        let (mut info, mut raw_info, mut seektable) = (None, Vec::new(), Vec::new());
        let mut pos = 4u64;
        loop {
            let h = w.at(pos, 4)?.to_vec();
            if h.len() < 4 {
                return Err(Error::Demux("flac: truncated metadata".into()));
            }
            let (last, kind, len) = (h[0] & 0x80 != 0, h[0] & 0x7F, u32::from_be_bytes([0, h[1], h[2], h[3]]) as u64);
            let body = pos + 4;
            match kind {
                0 => {
                    raw_info = w.at(body, 34)?.to_vec();
                    info = parse_streaminfo(&raw_info);
                }
                3 => seektable = parse_seektable(w.at(body, len as usize)?),
                4 if len <= MAX_PICTURE as u64 => tags::vorbis::parse_vorbis_comment(w.at(body, len as usize)?, &mut meta, &mut covers),
                6 if len <= MAX_PICTURE as u64 + 4096 => {
                    if let Some((front, mime, data)) = tags::vorbis::parse_flac_picture(w.at(body, len as usize)?) {
                        covers.offer(front, &mime, data);
                    }
                }
                _ => {}
            }
            pos = body + len;
            if last {
                break;
            }
        }
        covers.finish(&mut meta);
        let info = info.ok_or_else(|| Error::Demux("flac: no STREAMINFO".into()))?;
        let mut s = StreamInfo::new(0, StreamKind::Audio, Codec::Flac);
        s.sample_rate = info.rate;
        s.channels = info.channels;
        s.extradata = Some(flac_extradata(&raw_info));
        if info.total > 0 {
            s.duration = Some(Duration::from_secs_f64(info.total as f64 / info.rate as f64));
        }
        Ok(Self {
            end: w.len().unwrap_or(u64::MAX),
            w,
            streams: vec![s],
            metadata: (!meta.is_empty()).then_some(meta),
            info,
            seektable,
            first_frame: pos,
            pos,
        })
    }

    fn time(&self, sample: u64) -> Duration {
        Duration::from_secs_f64(sample as f64 / self.info.rate as f64)
    }

    /// The frame starting at `at` and its length: up to the next valid frame header at which the
    /// bytes so far pass the frame's CRC-16, or to the end of the file. `None` when `at` is not a
    /// frame header.
    fn frame_at(&mut self, at: u64) -> Result<Option<(FlacFrame, usize)>> {
        let head = self.w.at(at, 16)?.to_vec();
        let Some(frame) = flac_frame_header(&head, &self.info) else { return Ok(None) };
        let (mut crc, mut len) = (0u16, 0usize);
        loop {
            let chunk = self.w.at(at + len as u64, CHUNK)?.to_vec();
            if chunk.is_empty() {
                return Ok(Some((frame, len)));
            }
            for (i, &x) in chunk.iter().enumerate() {
                let off = len + i;
                // The CRC-16 of a whole frame, including its own CRC, is 0.
                if x == 0xFF && crc == 0 && off > frame.header_len + 2 {
                    let next = self.w.at(at + off as u64, 16)?.to_vec();
                    if flac_frame_header(&next, &self.info).is_some() {
                        return Ok(Some((frame, off)));
                    }
                }
                crc = crc16_update(crc, x);
            }
            len += chunk.len();
            if len > MAX_FRAME {
                return Err(Error::Demux("flac: frame end not found".into()));
            }
        }
    }

    /// The first frame header at or after `from`: (offset, first sample).
    fn resync(&mut self, from: u64) -> Result<Option<(u64, u64)>> {
        let mut at = from;
        loop {
            let chunk = self.w.at(at, CHUNK)?.to_vec();
            // Each step overlaps the next by 16 bytes (a frame header can straddle them); the
            // last, short chunk is scanned to its end.
            let last = chunk.len() < CHUNK;
            let scan = if last { chunk.len() } else { chunk.len() - 16 };
            for i in 0..scan {
                if chunk[i] == 0xFF
                    && let Some(f) = flac_frame_header(&chunk[i..], &self.info)
                {
                    return Ok(Some((at + i as u64, f.sample)));
                }
            }
            if last {
                return Ok(None);
            }
            at += scan as u64;
        }
    }

    /// Bisection over byte offsets: a frame start at or before the frame holding `goal`.
    fn bisect(&mut self, goal: u64) -> Result<u64> {
        let (mut lo, mut hi) = (self.first_frame, self.end);
        while hi.saturating_sub(lo) > CHUNK as u64 {
            let mid = lo + (hi - lo) / 2;
            match self.resync(mid)? {
                Some((at, sample)) if sample <= goal && at < hi => lo = at,
                _ => hi = mid,
            }
        }
        Ok(lo)
    }
}

impl Demuxer for FlacDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            match self.frame_at(self.pos)? {
                Some((frame, len)) if len > 0 => {
                    let data = self.w.at(self.pos, len)?.to_vec();
                    self.pos += len as u64;
                    return Ok(Some(Packet { stream: 0, pts: self.time(frame.sample), keyframe: true, data, generation: 0 }));
                }
                Some(_) => return Ok(None),
                None => match self.resync(self.pos + 1)? {
                    Some((at, _)) => self.pos = at,
                    None => return Ok(None),
                },
            }
        }
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        let goal = (target.as_secs_f64() * self.info.rate as f64) as u64;
        // A seek point is trusted only if it lands inside the file.
        let point = self.seektable.iter().rev().find(|(sample, _)| *sample <= goal);
        let mut at = match point.and_then(|&(_, offset)| self.first_frame.checked_add(offset)).filter(|&at| at < self.end) {
            Some(at) => at,
            None if self.w.is_seekable() && self.end != u64::MAX => self.bisect(goal)?,
            None => self.first_frame,
        };
        // Forward to the frame holding `goal`.
        let mut landed = 0;
        loop {
            match self.frame_at(at)? {
                Some((frame, len)) => {
                    landed = frame.sample;
                    if len == 0 || frame.sample + frame.block_size as u64 > goal {
                        break;
                    }
                    at += len as u64;
                }
                None => match self.resync(at)? {
                    Some((next, _)) => at = next,
                    None => break,
                },
            }
        }
        self.pos = at;
        Ok(self.time(landed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileSource;

    fn open() -> FlacDemuxer {
        FlacDemuxer::open(Box::new(FileSource::open("tests/fixtures/flac.flac").unwrap())).unwrap()
    }

    #[test]
    fn frames_are_split_at_their_ends_with_rising_pts() {
        let mut d = open();
        let mut last = None;
        let mut frames = 0;
        while let Some(p) = d.next_packet().unwrap() {
            assert!(flac_frame_header(&p.data, &d.info).is_some(), "each packet starts with a frame header");
            assert!(last.is_none_or(|l| p.pts > l));
            last = Some(p.pts);
            frames += 1;
        }
        assert!(frames > 10);
    }

    /// A seek table with a point every fifth frame, built by reading the file.
    fn scanned_seektable() -> Vec<(u64, u64)> {
        let mut d = open();
        let mut table = Vec::new();
        let mut frame = 0;
        loop {
            let at = d.pos;
            let Some(p) = d.next_packet().unwrap() else { break };
            if frame % 5 == 0 {
                table.push(((p.pts.as_secs_f64() * d.info.rate as f64).round() as u64, at - d.first_frame));
            }
            frame += 1;
        }
        table
    }

    #[test]
    fn bisection_and_seektable_land_on_the_same_frame() {
        let table = scanned_seektable();
        assert!(table.len() > 3);
        for t in [0, 700, 1500, 1990] {
            let t = Duration::from_millis(t);
            let (mut a, mut b) = (open(), open());
            a.seektable = table.clone();
            b.seektable.clear();
            assert_eq!(a.seek(t).unwrap(), b.seek(t).unwrap(), "{t:?}");
            assert_eq!(a.next_packet().unwrap().unwrap().pts, b.next_packet().unwrap().unwrap().pts);
        }
    }

    #[test]
    fn junk_after_the_metadata_ends_the_stream_without_hanging() {
        // fLaC + STREAMINFO (last block; 44.1 kHz stereo 16-bit) + 64 zero bytes: no frame anywhere.
        let mut bytes = b"fLaC\x80\x00\x00\x22".to_vec();
        let mut info = [0u8; 34];
        info[..4].copy_from_slice(&[0x10, 0x00, 0x10, 0x00]);
        info[10..14].copy_from_slice(&[0x0A, 0xC4, 0x42, 0xF0]);
        bytes.extend(info);
        bytes.extend([0u8; 64]);
        let path = std::env::temp_dir().join(format!("flac_junk_{}.flac", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut d = FlacDemuxer::open(Box::new(FileSource::open(&path).unwrap())).unwrap();
            let _ = tx.send(d.next_packet().map(|p| p.is_none()));
        });
        let ended = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("next_packet hung");
        assert!(ended.unwrap(), "no packet from junk");
    }

    #[test]
    fn a_seektable_point_past_the_file_falls_back_to_bisection() {
        let (mut a, mut b) = (open(), open());
        a.seektable = vec![(0, u64::MAX), (1, u64::MAX / 2)];
        b.seektable.clear();
        let t = Duration::from_millis(1500);
        assert_eq!(a.seek(t).unwrap(), b.seek(t).unwrap());
        assert_eq!(a.next_packet().unwrap().unwrap().pts, b.next_packet().unwrap().unwrap().pts);
    }

    #[test]
    fn metadata_running_past_the_end_of_the_file_is_survived() {
        // STREAMINFO, then a last (padding) block claiming 1 MB in a 100-byte file.
        let mut bytes = b"fLaC\x00\x00\x00\x22".to_vec();
        let mut info = [0u8; 34];
        info[..4].copy_from_slice(&[0x10, 0x00, 0x10, 0x00]);
        info[10..14].copy_from_slice(&[0x0A, 0xC4, 0x42, 0xF0]);
        bytes.extend(info);
        bytes.extend([0x81, 0x0F, 0x42, 0x40]);
        bytes.resize(100, 0);
        let path = std::env::temp_dir().join(format!("flac_past_end_{}.flac", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let mut d = FlacDemuxer::open(Box::new(FileSource::open(&path).unwrap())).unwrap();
        d.seek(Duration::from_secs(1)).unwrap();
        assert!(d.next_packet().unwrap().is_none());
    }

    #[test]
    fn seektable_points_skip_placeholders() {
        let mut b = Vec::new();
        for (sample, offset) in [(0u64, 0u64), (44_100, 9_000), (u64::MAX, 0)] {
            b.extend(sample.to_be_bytes());
            b.extend(offset.to_be_bytes());
            b.extend(4096u16.to_be_bytes());
        }
        assert_eq!(parse_seektable(&b), [(0, 0), (44_100, 9_000)]);
    }

    #[test]
    fn utf8_coded_numbers() {
        assert_eq!(utf8_number(&[0x7F]), Some((0x7F, 1)));
        assert_eq!(utf8_number(&[0xC2, 0x80]), Some((0x80, 2)));
        assert_eq!(utf8_number(&[0x80]), None);
        assert_eq!(utf8_number(&[0xE0, 0x40, 0x80]), None, "bad continuation byte");
    }
}
