//! Ogg: Vorbis, Opus or FLAC in the first logical stream. Packets are reassembled across pages.
//! Packet pts come from granule positions: worked back from a page's granule at the start and after
//! a seek, then counted forward. The last page's granule gives the duration and the end trim.

use std::collections::VecDeque;
use std::time::Duration;

use lewton::header::{IdentHeader, SetupHeader, read_header_ident, read_header_setup};

use super::flac::{FlacInfo, flac_extradata, flac_frame_header, parse_streaminfo};
use super::metadata::{CoverPick, MAX_PICTURE};
use super::window::ReadWindow;
use super::{Codec, Demuxer, Metadata, Packet, StreamInfo, StreamKind, tags};
use crate::source::MediaSource;
use crate::{Error, Result};

/// Bytes searched per step for a capture pattern (`OggS`).
const SEARCH: usize = 64 * 1024;
/// Header packets (comments with cover art) larger than this are refused.
const MAX_HEADER_PACKET: usize = MAX_PICTURE * 2;
const CONTINUED: u8 = 1;
const BOS: u8 = 2;

struct Page {
    #[allow(dead_code)] // read by seeking (Task 10)
    offset: u64,
    len: u64,
    flags: u8,
    /// −1 when no packet ends on this page.
    granule: i64,
    serial: u32,
    /// Packet data on this page, and whether the packet ends here.
    packets: Vec<(Vec<u8>, bool)>,
}

fn ogg_crc(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |mut c, &x| {
        c ^= (x as u32) << 24;
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 { c << 1 ^ 0x04C1_1DB7 } else { c << 1 };
        }
        c
    })
}

/// The page at `at`, when a valid one (capture pattern, version 0, CRC) starts there.
fn read_page(w: &mut ReadWindow, at: u64) -> Result<Option<Page>> {
    let head = w.at(at, 27)?.to_vec();
    if head.len() < 27 || &head[..4] != b"OggS" || head[4] != 0 {
        return Ok(None);
    }
    let segments = head[26] as usize;
    let lacing = w.at(at + 27, segments)?.to_vec();
    if lacing.len() < segments {
        return Ok(None);
    }
    let total = 27 + segments + lacing.iter().map(|&l| l as usize).sum::<usize>();
    let mut bytes = w.at(at, total)?.to_vec();
    if bytes.len() < total {
        return Ok(None);
    }
    let crc = u32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);
    bytes[22..26].fill(0);
    if ogg_crc(&bytes) != crc {
        return Ok(None);
    }
    let (mut packets, mut current, mut pos) = (Vec::new(), Vec::new(), 27 + segments);
    for &l in &lacing {
        current.extend_from_slice(&bytes[pos..pos + l as usize]);
        pos += l as usize;
        if l < 255 {
            packets.push((std::mem::take(&mut current), true));
        }
    }
    if lacing.last() == Some(&255) {
        packets.push((current, false));
    }
    Ok(Some(Page {
        offset: at,
        len: total as u64,
        flags: head[5],
        granule: i64::from_le_bytes(head[6..14].try_into().expect("8 bytes")),
        serial: u32::from_le_bytes(head[14..18].try_into().expect("4 bytes")),
        packets,
    }))
}

/// Header packets each codec starts with (identification packet first).
fn header_count(id: &[u8]) -> Option<usize> {
    if id.starts_with(b"\x01vorbis") {
        Some(3)
    } else if id.starts_with(b"OpusHead") {
        Some(2)
    } else if id.starts_with(b"\x7FFLAC") && id.len() >= 9 {
        Some(1 + u16::from_be_bytes([id[7], id[8]]).max(1) as usize)
    } else {
        None
    }
}

/// Samples (at 48 kHz) in an Opus packet, from its TOC byte (RFC 6716 3.1).
fn opus_samples(p: &[u8]) -> u64 {
    let Some(&toc) = p.first() else { return 0 };
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config & 3) as usize],
        12..=15 => [480, 960][(config & 1) as usize],
        _ => [120, 240, 480, 960][(config & 3) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => p.get(1).map_or(0, |b| (b & 0x3F) as u64),
    };
    frame * frames
}

/// The Vorbis decoder's setup bytes (Matroska CodecPrivate layout): the headers, Xiph-laced.
fn xiph_lace(headers: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![(headers.len() - 1) as u8];
    for h in &headers[..headers.len() - 1] {
        let mut n = h.len();
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    for h in headers {
        out.extend(h);
    }
    out
}

/// The granule of this stream's last page with one, searching back from the end.
fn last_granule(w: &mut ReadWindow, serial: u32, end: u64) -> Result<Option<u64>> {
    let mut from = end.saturating_sub(SEARCH as u64);
    loop {
        let chunk = w.at(from, SEARCH)?.to_vec();
        let mut found = None;
        let mut i = 0;
        while i + 4 <= chunk.len() {
            let Some(off) = chunk[i..].windows(4).position(|x| x == b"OggS") else { break };
            if let Some(p) = read_page(w, from + (i + off) as u64)?
                && p.serial == serial
                && p.granule >= 0
            {
                found = Some(p.granule as u64);
            }
            i += off + 1;
        }
        if found.is_some() || from == 0 {
            return Ok(found);
        }
        from = from.saturating_sub(SEARCH as u64 - 64);
    }
}

enum OggCodec {
    Vorbis { ident: Box<IdentHeader>, setup: Box<SetupHeader> },
    #[allow(dead_code)] // `pre_skip` is read by seeking (Task 10)
    Opus { pre_skip: u64 },
    Flac { info: FlacInfo },
}

pub struct OggDemuxer {
    w: ReadWindow,
    streams: Vec<StreamInfo>,
    metadata: Option<Metadata>,
    serial: u32,
    codec: OggCodec,
    rate: u32,
    /// Next page to read.
    pos: u64,
    first_audio_page: u64,
    end: u64,
    ready: VecDeque<Packet>,
    /// A packet that continues on the next page.
    partial: Vec<u8>,
    /// After a seek: the packet continued onto the landing page began before it; drop it.
    skip_partial: bool,
    /// Granule where the next packet starts, once known (then counted forward).
    next_start: Option<u64>,
    /// Vorbis: the previous packet's block size (`None` at the start and after a seek).
    prev_block: Option<u64>,
    done: bool,
}

impl OggDemuxer {
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut w = ReadWindow::new(src);
        let first = read_page(&mut w, 0)?.filter(|p| p.flags & BOS != 0).ok_or(Error::UnsupportedContainer)?;
        let serial = first.serial;
        // Assemble the header packets (they may span pages; the first audio packet starts a page).
        let (mut headers, mut partial, mut needed, mut pos) = (Vec::<Vec<u8>>::new(), Vec::new(), 1usize, 0u64);
        while headers.len() < needed {
            let page = read_page(&mut w, pos)?.ok_or_else(|| Error::Demux("ogg: truncated headers".into()))?;
            pos += page.len;
            if page.serial != serial {
                continue;
            }
            let continued = page.flags & CONTINUED != 0;
            for (i, (data, done)) in page.packets.into_iter().enumerate() {
                let mut packet = if i == 0 && continued { std::mem::take(&mut partial) } else { Vec::new() };
                if packet.len() + data.len() > MAX_HEADER_PACKET {
                    return Err(Error::Demux("ogg: header packet too large".into()));
                }
                packet.extend(data);
                if !done {
                    partial = packet;
                    continue;
                }
                if headers.is_empty() {
                    needed = header_count(&packet).ok_or(Error::Unsupported("Ogg stream: only Vorbis, Opus and FLAC are supported"))?;
                }
                if headers.len() < needed {
                    headers.push(packet);
                }
            }
        }
        let end = w.len().unwrap_or(u64::MAX);
        let last = if end != u64::MAX && w.is_seekable() { last_granule(&mut w, serial, end)? } else { None };
        let (mut meta, mut covers) = (Metadata::default(), CoverPick::default());
        let secs = |samples: u64, rate: u32| Duration::from_secs_f64(samples as f64 / rate as f64);
        let id = &headers[0];
        let (codec, mut s, rate) = if id.starts_with(b"\x01vorbis") {
            let err = |e| Error::Decode(format!("vorbis header: {e:?}"));
            let ident = read_header_ident(&headers[0]).map_err(err)?;
            let setup = read_header_setup(&headers[2], ident.audio_channels, (ident.blocksize_0, ident.blocksize_1)).map_err(err)?;
            if let Some(c) = headers[1].strip_prefix(b"\x03vorbis") {
                tags::vorbis::parse_vorbis_comment(c, &mut meta, &mut covers);
            }
            let rate = ident.audio_sample_rate;
            let mut s = StreamInfo::new(0, StreamKind::Audio, Codec::Vorbis);
            s.sample_rate = rate;
            s.channels = ident.audio_channels as u16;
            s.extradata = Some(xiph_lace(&headers));
            // ffmpeg trims a Vorbis stream to its last granule as well.
            s.end_trim = last.map(|g| secs(g, rate));
            (OggCodec::Vorbis { ident: Box::new(ident), setup: Box::new(setup) }, s, rate)
        } else if id.starts_with(b"OpusHead") {
            if id.len() < 19 {
                return Err(Error::Demux("ogg: short OpusHead".into()));
            }
            let pre_skip = u16::from_le_bytes([id[10], id[11]]) as u64;
            if let Some(c) = headers[1].strip_prefix(b"OpusTags") {
                tags::vorbis::parse_vorbis_comment(c, &mut meta, &mut covers);
            }
            let mut s = StreamInfo::new(0, StreamKind::Audio, Codec::Opus);
            s.sample_rate = 48_000;
            s.channels = id[9] as u16;
            s.extradata = Some(id.clone());
            s.codec_delay = secs(pre_skip, 48_000);
            s.seek_preroll = Duration::from_millis(80);
            s.end_trim = last.map(|g| secs(g.saturating_sub(pre_skip), 48_000));
            (OggCodec::Opus { pre_skip }, s, 48_000)
        } else {
            let raw = id.get(17..51).ok_or_else(|| Error::Demux("ogg: short FLAC mapping header".into()))?;
            let info = parse_streaminfo(raw).ok_or_else(|| Error::Demux("ogg: bad FLAC STREAMINFO".into()))?;
            for block in &headers[1..] {
                let body = block.get(4..).unwrap_or(&[]);
                match block.first().map(|b| b & 0x7F) {
                    Some(4) => tags::vorbis::parse_vorbis_comment(body, &mut meta, &mut covers),
                    Some(6) => {
                        if let Some((front, mime, data)) = tags::vorbis::parse_flac_picture(body) {
                            covers.offer(front, &mime, data);
                        }
                    }
                    _ => {}
                }
            }
            let mut s = StreamInfo::new(0, StreamKind::Audio, Codec::Flac);
            s.sample_rate = info.rate;
            s.channels = info.channels;
            s.extradata = Some(flac_extradata(raw));
            let rate = info.rate;
            (OggCodec::Flac { info }, s, rate)
        };
        covers.finish(&mut meta);
        s.duration = s.end_trim.or_else(|| last.map(|g| secs(g, rate)));
        Ok(Self {
            w,
            streams: vec![s],
            metadata: (!meta.is_empty()).then_some(meta),
            serial,
            codec,
            rate,
            pos,
            first_audio_page: pos,
            end,
            ready: VecDeque::new(),
            partial: Vec::new(),
            skip_partial: false,
            next_start: None,
            prev_block: None,
            done: false,
        })
    }

    fn time(&self, granule: u64) -> Duration {
        Duration::from_secs_f64(granule as f64 / self.rate as f64)
    }

    /// Samples a packet decodes to, in granule units.
    fn samples_in(&mut self, p: &[u8]) -> u64 {
        match &self.codec {
            OggCodec::Opus { .. } => opus_samples(p),
            OggCodec::Flac { info } => flac_frame_header(p, info).map_or(0, |f| f.block_size as u64),
            OggCodec::Vorbis { ident, setup } => {
                let (short, long) = (1u64 << ident.blocksize_0, 1u64 << ident.blocksize_1);
                let Ok(n) = lewton::audio::get_decoded_sample_count(ident, setup, p) else { return 0 };
                // lewton reports this block's window span; a short block's is short/2.
                let cur = if n as u64 == short / 2 && short != long { short } else { long };
                // A Vorbis packet returns prev/4 + cur/4 samples; the first one returns none.
                let count = self.prev_block.map_or(0, |prev| prev / 4 + cur / 4);
                self.prev_block = Some(cur);
                count
            }
        }
    }

    /// The next page at `pos`, resyncing to the next capture pattern over damaged data.
    fn next_page(&mut self) -> Result<Option<Page>> {
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            if let Some(p) = read_page(&mut self.w, self.pos)? {
                self.pos += p.len;
                return Ok(Some(p));
            }
            let chunk = self.w.at(self.pos + 1, SEARCH)?.to_vec();
            match chunk.windows(4).position(|x| x == b"OggS") {
                Some(i) => self.pos += 1 + i as u64,
                None if chunk.len() < SEARCH => return Ok(None),
                None => self.pos += (SEARCH - 3) as u64,
            }
        }
    }

    /// Completes the page's packets and queues them with their pts.
    fn take_page(&mut self, page: Page) {
        let continued = page.flags & CONTINUED != 0;
        let mut complete = Vec::new();
        for (i, (data, done)) in page.packets.into_iter().enumerate() {
            if i == 0 && continued && self.skip_partial {
                // The rest of a packet that began before the seek landing: incomplete, dropped.
                if done {
                    self.skip_partial = false;
                }
                continue;
            }
            if i == 0 {
                self.skip_partial = false;
            }
            let mut packet = if i == 0 && continued { std::mem::take(&mut self.partial) } else { Vec::new() };
            packet.extend(data);
            if done {
                complete.push(packet);
            } else {
                self.partial = packet;
            }
        }
        if complete.is_empty() {
            return;
        }
        let counts: Vec<u64> = complete.iter().map(|p| self.samples_in(p)).collect();
        let starts: Vec<u64> = match self.next_start {
            Some(mut at) => counts
                .iter()
                .map(|c| {
                    let start = at;
                    at += c;
                    start
                })
                .collect(),
            None => {
                // Work back from the page's granule (the end of its last complete packet).
                let mut end = page.granule.max(0) as u64;
                let mut starts: Vec<u64> = counts
                    .iter()
                    .rev()
                    .map(|c| {
                        end = end.saturating_sub(*c);
                        end
                    })
                    .collect();
                starts.reverse();
                starts
            }
        };
        self.next_start = starts.last().zip(counts.last()).map(|(s, c)| s + c);
        for (data, start) in complete.into_iter().zip(starts) {
            self.ready.push_back(Packet { stream: 0, pts: self.time(start), keyframe: true, data, generation: 0 });
        }
    }

    /// Back to the first audio page with fresh packet state.
    fn restart_at(&mut self, page: u64) {
        self.pos = page;
        self.ready.clear();
        self.partial.clear();
        self.skip_partial = page != self.first_audio_page;
        self.next_start = None;
        self.prev_block = None;
        self.done = false;
    }
}

impl Demuxer for OggDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Ok(Some(p));
            }
            if self.done {
                return Ok(None);
            }
            match self.next_page()? {
                None => self.done = true,
                // A chained stream begins: only the first one plays.
                Some(page) if page.flags & BOS != 0 => self.done = true,
                Some(page) if page.serial == self.serial => self.take_page(page),
                Some(_) => {}
            }
        }
    }

    fn seek(&mut self, _target: Duration) -> Result<Duration> {
        // Task 10 replaces this with bisection.
        self.restart_at(self.first_audio_page);
        Ok(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_packet_durations_from_the_toc_byte() {
        assert_eq!(opus_samples(&[0xFC]), 960, "CELT 20 ms, one frame");
        assert_eq!(opus_samples(&[0x19, 0]), 2880 * 2, "SILK 60 ms, two frames");
        assert_eq!(opus_samples(&[0x93, 0x03]), 480 * 3, "CELT 10 ms, code 3 with 3 frames");
        assert_eq!(opus_samples(&[]), 0);
    }

    #[test]
    fn xiph_lacing_round_trips_through_the_vorbis_decoder_splitter() {
        let headers = vec![vec![1u8; 30], vec![3u8; 300], vec![5u8; 10]];
        let laced = xiph_lace(&headers);
        let split = crate::demux::split_xiph_lacing(&laced).unwrap();
        assert_eq!(split, headers.iter().map(Vec::as_slice).collect::<Vec<_>>());
    }

    #[test]
    fn crc_of_a_known_page() {
        // An Ogg page's CRC is computed with the CRC field zeroed; check it on a fixture's first page.
        let bytes = std::fs::read("tests/fixtures/opus.opus").unwrap();
        let len = 27 + bytes[26] as usize + bytes[27..27 + bytes[26] as usize].iter().map(|&l| l as usize).sum::<usize>();
        let mut page = bytes[..len].to_vec();
        let crc = u32::from_le_bytes(page[22..26].try_into().unwrap());
        page[22..26].fill(0);
        assert_eq!(ogg_crc(&page), crc);
    }
}
