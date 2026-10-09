//! MPEG transport streams (ISO 13818-1): `.ts`/`.m2ts` files and HLS segments.
//!
//! Video (H.264, HEVC) arrives as Annex B and leaves as 4-byte length-prefixed NAL units with an
//! `avcC`/`hvcC` record built from the in-band parameter sets, as MP4 delivers it, so every
//! decoder sees what it already handles. Audio: AAC (ADTS, one packet per frame, header removed),
//! MP3 (per frame), AC-3 and E-AC-3 (per PES).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::mpeg_audio::{AdtsHeader, MpegHeader};
use super::window::ReadWindow;
use super::{Codec, Demuxer, Packet, StreamInfo, StreamKind};
use crate::nal;
use crate::source::{MediaSource, MemorySource};
use crate::{Error, Result};

pub(crate) const TS_PACKET: usize = 188;
const WRAP: u64 = 1 << 33;
const CLOCK: u64 = 90_000;
/// Bytes read at open to find the format of every stream.
const OPEN_SCAN: u64 = 4 << 20;
/// Bytes at the end of a local file searched for the last timestamp.
const TAIL_SCAN: u64 = 1 << 20;
/// Bytes searched for a timestamp at a bisection point.
const PROBE_SCAN: u64 = 512 * 1024;
/// A seek starts demuxing this long before the target, to find the keyframe before it.
const SEEK_MARGIN: u64 = 3 * CLOCK;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Es {
    H264,
    Hevc,
    Adts,
    Mpeg,
    Ac3,
    Eac3,
}

impl Es {
    fn is_video(self) -> bool {
        matches!(self, Es::H264 | Es::Hevc)
    }
}

/// One elementary stream's reassembly and codec state.
struct Pid {
    es: Es,
    /// Index into `streams` once the format is known.
    stream: Option<usize>,
    pes: Vec<u8>,
    active: bool,
    cc: Option<u8>,
    /// Video parameter sets seen last: VPS (HEVC), SPS, PPS.
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// Audio bytes not yet cut into frames, and the time of the next frame (90 kHz, fractional).
    pending: Vec<u8>,
    next_pts: Option<f64>,
    last_pts: Option<u64>,
}

impl Pid {
    fn new(es: Es) -> Self {
        Self { es, stream: None, pes: Vec::new(), active: false, cc: None, vps: None, sps: None, pps: None, pending: Vec::new(), next_pts: None, last_pts: None }
    }

    fn reset(&mut self) {
        self.pes.clear();
        self.active = false;
        self.cc = None;
        self.pending.clear();
        self.next_pts = None;
    }
}

/// A demuxed packet with its timestamp still in unwrapped 90 kHz ticks.
struct Raw {
    stream: u32,
    ticks: u64,
    keyframe: bool,
    data: Vec<u8>,
    /// The stream's new format, starting with this packet.
    update: Option<StreamInfo>,
}

pub struct TsDemuxer {
    w: ReadWindow,
    pos: u64,
    /// 188, or 192 for `.m2ts` (a 4-byte timecode before each packet).
    packet_size: usize,
    pmt_pid: Option<u16>,
    pmt_seen: bool,
    pids: HashMap<u16, Pid>,
    streams: Vec<StreamInfo>,
    out: VecDeque<Raw>,
    update: Option<StreamInfo>,
    /// Unwrapping reference: the last timestamp seen (unwrapped ticks).
    reference: Option<u64>,
    last_raw: Option<u64>,
    /// Files: the first timestamp, presented as 0. Segments keep raw times (`None`).
    zero: Option<u64>,
    file: bool,
    ended: bool,
}

impl TsDemuxer {
    /// A `.ts`/`.m2ts` file: timestamps start at 0; duration and seeking from the file.
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut d = Self::new(ReadWindow::new(src), true, None)?;
        d.zero = d.out.iter().map(|r| r.ticks).min().or(d.last_raw);
        if let Some(duration) = d.file_duration()? {
            for s in &mut d.streams {
                s.duration = Some(duration);
            }
        }
        Ok(d)
    }

    /// An HLS segment in memory. Timestamps stay raw (unwrapped 90 kHz ticks as `Duration`),
    /// unwrapped near `reference` (the previous segment's [`last_raw_pts`](Self::last_raw_pts)).
    pub fn open_segment(data: Vec<u8>, reference: Option<u64>) -> Result<Self> {
        Self::new(ReadWindow::new(Box::new(MemorySource::new(data, "TS segment"))), false, reference)
    }

    /// The highest timestamp seen (unwrapped ticks), to continue unwrapping in the next segment.
    pub fn last_raw_pts(&self) -> Option<u64> {
        self.last_raw
    }

    fn new(mut w: ReadWindow, file: bool, reference: Option<u64>) -> Result<Self> {
        let head = w.at(0, 4 + 3 * 192)?.to_vec();
        let packet_size = packet_size(&head).ok_or_else(|| Error::Demux("not an MPEG transport stream".into()))?;
        let mut d = Self {
            w,
            pos: 0,
            packet_size,
            pmt_pid: None,
            pmt_seen: false,
            pids: HashMap::new(),
            streams: Vec::new(),
            out: VecDeque::new(),
            update: None,
            reference,
            last_raw: None,
            zero: None,
            file,
            ended: false,
        };
        // Read until every stream's format is known.
        while !d.pmt_seen || d.pids.values().any(|p| p.stream.is_none()) {
            if d.pos > OPEN_SCAN || !d.step()? {
                break;
            }
        }
        if d.streams.is_empty() {
            return Err(Error::Demux("MPEG-TS without a playable stream".into()));
        }
        // Streams whose format never showed up are left out.
        let known: Vec<u16> = d.pids.iter().filter(|(_, p)| p.stream.is_none()).map(|(&pid, _)| pid).collect();
        for pid in known {
            d.pids.remove(&pid);
        }
        Ok(d)
    }

    fn ticks_to_time(&self, ticks: u64) -> Duration {
        let t = ticks.saturating_sub(self.zero.unwrap_or(0));
        Duration::from_nanos((t as u128 * 1_000_000_000 / CLOCK as u128) as u64)
    }

    /// The next TS packet (188 bytes), resynchronising on a lost sync byte.
    fn read_packet(&mut self) -> Result<Option<[u8; TS_PACKET]>> {
        let skip = self.packet_size - TS_PACKET;
        loop {
            let buf = self.w.at(self.pos, self.packet_size)?;
            if buf.len() < self.packet_size {
                return Ok(None);
            }
            if buf[skip] == 0x47 {
                let p: [u8; TS_PACKET] = buf[skip..].try_into().unwrap();
                self.pos += self.packet_size as u64;
                return Ok(Some(p));
            }
            let window = self.w.at(self.pos + 1, 64 * 1024)?.to_vec();
            let ps = self.packet_size;
            match (0..window.len()).find(|&i| (0..3).all(|k| window.get(i + skip + k * ps) == Some(&0x47))) {
                Some(i) => self.pos += 1 + i as u64,
                None if window.len() < 3 * ps => return Ok(None),
                None => self.pos += window.len() as u64,
            }
        }
    }

    /// Processes one TS packet; `false` at the end of the input (open PES packets are finished).
    fn step(&mut self) -> Result<bool> {
        let Some(p) = self.read_packet()? else {
            if !self.ended {
                self.ended = true;
                let open: Vec<u16> = self.pids.iter().filter(|(_, s)| s.active).map(|(&pid, _)| pid).collect();
                for pid in open {
                    self.finish(pid);
                }
            }
            return Ok(false);
        };
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        let start = p[1] & 0x40 != 0;
        let control = (p[3] >> 4) & 3;
        let cc = p[3] & 0x0F;
        let mut at = 4;
        if control & 2 != 0 {
            at += 1 + p[4] as usize;
        }
        if control & 1 == 0 || at > TS_PACKET {
            return Ok(true);
        }
        let payload = &p[at..];
        if pid == 0 {
            if start {
                self.on_pat(payload);
            }
        } else if Some(pid) == self.pmt_pid {
            if start {
                self.on_pmt(payload);
            }
        } else if let Some(s) = self.pids.get_mut(&pid) {
            if let Some(last) = s.cc {
                if cc == last {
                    return Ok(true); // a duplicate
                }
                if cc != (last + 1) & 0x0F {
                    s.pes.clear();
                    s.active = false;
                }
            }
            s.cc = Some(cc);
            if start {
                if s.active {
                    self.finish(pid);
                }
                let s = self.pids.get_mut(&pid).unwrap();
                s.pes.clear();
                s.pes.extend_from_slice(payload);
                s.active = true;
            } else if s.active {
                s.pes.extend_from_slice(payload);
            }
            let s = self.pids.get_mut(&pid).unwrap();
            if s.active && s.pes.len() >= 6 {
                let len = u16::from_be_bytes([s.pes[4], s.pes[5]]) as usize;
                if len > 0 && s.pes.len() >= 6 + len {
                    self.finish(pid);
                }
            }
        }
        Ok(true)
    }

    fn on_pat(&mut self, payload: &[u8]) {
        let Some(t) = section(payload, 0) else { return };
        if let Some(pmt) = t.as_chunks::<4>().0.iter().find(|e| u16::from_be_bytes([e[0], e[1]]) != 0) {
            self.pmt_pid = Some(u16::from_be_bytes([pmt[2] & 0x1F, pmt[3]]));
        }
    }

    fn on_pmt(&mut self, payload: &[u8]) {
        if self.pmt_seen {
            return;
        }
        let Some(t) = section(payload, 2) else { return };
        self.pmt_seen = true;
        let Some(info_len) = t.get(2..4).map(|b| (u16::from_be_bytes([b[0], b[1]]) & 0x0FFF) as usize) else { return };
        let mut i = 4 + info_len;
        while i + 5 <= t.len() {
            let ty = t[i];
            let pid = u16::from_be_bytes([t[i + 1] & 0x1F, t[i + 2]]);
            let es_len = (u16::from_be_bytes([t[i + 3], t[i + 4]]) & 0x0FFF) as usize;
            let descriptors = t.get(i + 5..i + 5 + es_len).unwrap_or_default();
            let es = match ty {
                0x1B => Some(Es::H264),
                0x24 => Some(Es::Hevc),
                0x0F => Some(Es::Adts),
                0x03 | 0x04 => Some(Es::Mpeg),
                0x81 => Some(Es::Ac3),
                0x87 => Some(Es::Eac3),
                // DVB: private data with an AC-3 / E-AC-3 descriptor.
                0x06 => descriptor_tags(descriptors).find_map(|tag| match tag {
                    0x6A => Some(Es::Ac3),
                    0x7A => Some(Es::Eac3),
                    _ => None,
                }),
                _ => None,
            };
            // One video and one audio stream of each kind is plenty; keep all, the player picks.
            if let Some(es) = es {
                self.pids.insert(pid, Pid::new(es));
            }
            i += 5 + es_len;
        }
    }

    /// Unwraps a 33-bit timestamp to the value closest to the last one seen.
    fn unwrap_pts(&mut self, raw: u64) -> u64 {
        let v = match self.reference {
            None => raw,
            Some(r) => {
                let base = r - r % WRAP;
                [base.wrapping_sub(WRAP).wrapping_add(raw), base + raw, base + raw + WRAP]
                    .into_iter()
                    .filter(|&c| c < u64::MAX / 2)
                    .min_by_key(|&c| c.abs_diff(r))
                    .unwrap_or(raw)
            }
        };
        self.reference = Some(v);
        self.last_raw = Some(self.last_raw.map_or(v, |l| l.max(v)));
        v
    }

    /// A PES packet of `pid` is complete.
    fn finish(&mut self, pid: u16) {
        let Some(s) = self.pids.get_mut(&pid) else { return };
        s.active = false;
        let pes = std::mem::take(&mut s.pes);
        let Some((pts, payload)) = parse_pes(&pes) else { return };
        let pts = pts.map(|p| self.unwrap_pts(p));
        let s = self.pids.get_mut(&pid).unwrap();
        if pts.is_some() {
            s.last_pts = pts;
        }
        match s.es {
            Es::H264 | Es::Hevc => self.on_video(pid, pts, payload),
            Es::Adts | Es::Mpeg => self.on_frames(pid, pts, payload),
            Es::Ac3 | Es::Eac3 => self.on_ac3(pid, pts, payload),
        }
    }

    /// Registers or updates the stream of `pid` with `info`; returns the update to attach to the
    /// next packet, if the format changed.
    fn set_stream(&mut self, pid: u16, mut info: StreamInfo) -> Option<StreamInfo> {
        info.id = pid as u32;
        let s = self.pids.get_mut(&pid)?;
        match s.stream {
            None => {
                s.stream = Some(self.streams.len());
                self.streams.push(info);
                None
            }
            Some(i) => {
                let old = &self.streams[i];
                if old.codec == info.codec
                    && old.extradata == info.extradata
                    && (old.width, old.height, old.sample_rate, old.channels) == (info.width, info.height, info.sample_rate, info.channels)
                {
                    return None;
                }
                info.duration = old.duration;
                self.streams[i] = info.clone();
                Some(info)
            }
        }
    }

    fn on_video(&mut self, pid: u16, pts: Option<u64>, payload: &[u8]) {
        let s = self.pids.get_mut(&pid).unwrap();
        let hevc = s.es == Es::Hevc;
        let mut data = Vec::with_capacity(payload.len() + 16);
        let mut keyframe = false;
        for n in nal::split_annex_b(payload) {
            let ty = if hevc { (n[0] >> 1) & 0x3F } else { n[0] & 0x1F };
            match (hevc, ty) {
                (false, 9) | (true, 35) => continue, // access unit delimiters
                (false, 7) | (true, 33) => s.sps = Some(n.to_vec()),
                (false, 8) | (true, 34) => s.pps = Some(n.to_vec()),
                (true, 32) => s.vps = Some(n.to_vec()),
                (false, 5) | (true, 16..=23) => keyframe = true,
                _ => {}
            }
            data.extend_from_slice(&(n.len() as u32).to_be_bytes());
            data.extend_from_slice(n);
        }
        let config = match (hevc, &s.vps, &s.sps, &s.pps) {
            (false, _, Some(sps), Some(pps)) => {
                Some((nal::avcc_from(sps, pps), nal::h264_sps_size(sps), Codec::H264))
            }
            (true, Some(vps), Some(sps), Some(pps)) => {
                Some((nal::hvcc_from(vps, sps, pps), nal::hevc_sps_size(sps), Codec::Hevc))
            }
            _ => None,
        };
        let ticks = pts.or(s.last_pts);
        let mut update = None;
        if let Some((extradata, size, codec)) = config {
            let mut info = StreamInfo::new(0, StreamKind::Video, codec);
            (info.width, info.height) = size.unwrap_or_default();
            info.extradata = Some(extradata);
            update = self.set_stream(pid, info);
        }
        let s = &self.pids[&pid];
        if s.stream.is_none() || data.is_empty() {
            return; // nothing decodable before the parameter sets
        }
        let Some(ticks) = ticks else { return };
        self.out.push_back(Raw { stream: pid as u32, ticks, keyframe, data, update });
    }

    /// AAC (ADTS) and MP3: cut into frames, each its own packet.
    fn on_frames(&mut self, pid: u16, pts: Option<u64>, payload: &[u8]) {
        let s = self.pids.get_mut(&pid).unwrap();
        if let Some(pts) = pts
            && (s.pending.is_empty() || s.next_pts.is_none())
        {
            s.next_pts = Some(pts as f64);
        }
        s.pending.extend_from_slice(payload);
        let adts = s.es == Es::Adts;
        let mut pos = 0;
        let mut frames = Vec::new();
        loop {
            let s = &self.pids[&pid];
            let rest = &s.pending[pos..];
            if rest.len() < 7 {
                break;
            }
            let parsed = if adts {
                AdtsHeader::parse(rest).filter(|h| h.blocks == 1).map(|h| {
                    let mut info = StreamInfo::new(0, StreamKind::Audio, Codec::Aac);
                    info.sample_rate = h.sample_rate;
                    info.channels = match h.channel_config {
                        0 => 2,
                        7 => 8,
                        c => c as u16,
                    };
                    info.extradata = Some(h.audio_specific_config());
                    (h.frame_len, h.header_len, 1024, info)
                })
            } else {
                MpegHeader::parse(rest).map(|h| {
                    let mut info = StreamInfo::new(0, StreamKind::Audio, Codec::Mp3);
                    info.sample_rate = h.sample_rate;
                    info.channels = h.channels;
                    (h.frame_len, 0, h.samples, info)
                })
            };
            let Some((len, header, samples, info)) = parsed else {
                pos += 1; // resync
                continue;
            };
            if rest.len() < len {
                break;
            }
            frames.push((rest[header..len].to_vec(), samples, info));
            pos += len;
        }
        let s = self.pids.get_mut(&pid).unwrap();
        s.pending.drain(..pos);
        for (data, samples, info) in frames {
            let rate = info.sample_rate.max(1);
            let update = self.set_stream(pid, info);
            let s = self.pids.get_mut(&pid).unwrap();
            let Some(t) = s.next_pts else { continue };
            s.next_pts = Some(t + samples as f64 * CLOCK as f64 / rate as f64);
            self.out.push_back(Raw { stream: pid as u32, ticks: t.round() as u64, keyframe: true, data, update });
        }
    }

    /// AC-3 / E-AC-3: one packet per PES.
    fn on_ac3(&mut self, pid: u16, pts: Option<u64>, payload: &[u8]) {
        let eac3 = self.pids[&pid].es == Es::Eac3;
        let Some(info) = ac3_info(payload, eac3) else { return };
        let update = self.set_stream(pid, info);
        let Some(ticks) = pts.or(self.pids[&pid].last_pts) else { return };
        self.out.push_back(Raw { stream: pid as u32, ticks, keyframe: true, data: payload.to_vec(), update });
    }

    /// Duration of a local file from its last timestamp.
    fn file_duration(&mut self) -> Result<Option<Duration>> {
        let (Some(len), Some(zero)) = (self.w.len(), self.zero) else { return Ok(None) };
        if !self.w.is_local() {
            return Ok(None);
        }
        let Some(primary) = self.primary_pid() else { return Ok(None) };
        let from = len.saturating_sub(TAIL_SCAN);
        let mut last = None;
        let mut pos = from;
        while let Some((at, pts)) = self.pts_from(pos, len - pos, primary)? {
            last = Some(pts);
            pos = at + self.packet_size as u64;
        }
        Ok(last.map(|pts| {
            let pts = if pts < zero % WRAP { pts + WRAP } else { pts } + zero - zero % WRAP;
            self.ticks_to_time(pts)
        }))
    }

    /// The stream timestamps are searched in: the video, else the first audio.
    fn primary_pid(&self) -> Option<u16> {
        let first = |video: bool| {
            self.pids.iter().filter(|(_, p)| p.es.is_video() == video && p.stream.is_some()).map(|(&pid, _)| pid).min()
        };
        first(true).or_else(|| first(false))
    }

    /// The first PES start of `pid` with a PTS within `limit` bytes of `from`: its position and
    /// raw 33-bit PTS.
    fn pts_from(&mut self, from: u64, limit: u64, pid: u16) -> Result<Option<(u64, u64)>> {
        let ps = self.packet_size as u64;
        let skip = self.packet_size - TS_PACKET;
        let mut pos = from;
        // Find packet alignment first.
        let head = self.w.at(pos, (3 * ps + ps) as usize)?.to_vec();
        match (0..ps as usize).find(|&i| (0..3).all(|k| head.get(i + skip + k * ps as usize) == Some(&0x47))) {
            Some(i) => pos += i as u64,
            None => return Ok(None),
        }
        while pos < from + limit {
            let p = self.w.at(pos, self.packet_size)?.to_vec();
            if p.len() < self.packet_size || p[skip] != 0x47 {
                return Ok(None);
            }
            let p = &p[skip..];
            let this = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
            if this == pid && p[1] & 0x40 != 0 && (p[3] >> 4) & 1 != 0 {
                let at = if (p[3] >> 4) & 2 != 0 { 5 + p[4] as usize } else { 4 };
                if let Some((Some(pts), _)) = p.get(at..).and_then(parse_pes) {
                    return Ok(Some((pos, pts)));
                }
            }
            pos += ps;
        }
        Ok(None)
    }

    /// Resets parsing state to continue at `pos` (a packet boundary).
    fn reposition(&mut self, pos: u64, reference: u64) {
        self.pos = pos;
        self.out.clear();
        self.update = None;
        self.ended = false;
        self.reference = Some(reference);
        for p in self.pids.values_mut() {
            p.reset();
        }
    }

    /// Next demuxed packet (raw ticks), demuxing more input as needed.
    fn next_raw(&mut self) -> Result<Option<Raw>> {
        loop {
            if let Some(r) = self.out.pop_front() {
                return Ok(Some(r));
            }
            if !self.step()? && self.out.is_empty() {
                return Ok(None);
            }
        }
    }
}

impl Demuxer for TsDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let Some(r) = self.next_raw()? else { return Ok(None) };
        self.update = r.update;
        Ok(Some(Packet { stream: r.stream, pts: self.ticks_to_time(r.ticks), keyframe: r.keyframe, data: r.data, generation: 0 }))
    }

    fn take_stream_update(&mut self) -> Option<StreamInfo> {
        self.update.take()
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        if !self.file {
            return Err(Error::Seek("MPEG-TS segments are not seekable".into()));
        }
        let (Some(len), Some(zero), Some(primary)) = (self.w.len(), self.zero, self.primary_pid()) else {
            return Err(Error::Seek("MPEG-TS of unknown length".into()));
        };
        let video = self.pids[&primary].es.is_video();
        let target_ticks = zero + (target.as_nanos() * CLOCK as u128 / 1_000_000_000) as u64;
        let mut margin = if video { SEEK_MARGIN } else { 0 };
        loop {
            let goal = target_ticks.saturating_sub(margin);
            // Bisection: the last position whose timestamp is at or before `goal`.
            let (mut lo, mut hi) = (0u64, len);
            let mut start = 0u64;
            while hi - lo > 64 * 1024 {
                let mid = lo + (hi - lo) / 2;
                match self.pts_from(mid, PROBE_SCAN, primary)? {
                    Some((at, pts)) => {
                        let pts = if pts < zero % WRAP { pts + WRAP } else { pts } + zero - zero % WRAP;
                        if pts <= goal {
                            start = at;
                            lo = mid;
                        } else {
                            hi = mid;
                        }
                    }
                    None => hi = mid,
                }
            }
            self.reposition(start, goal);
            // Demux forward, keeping everything from the last keyframe at or before the target.
            let mut kept: Vec<Raw> = Vec::new();
            let mut key = None;
            while let Some(r) = self.next_raw()? {
                let primary_packet = r.stream == primary as u32;
                if primary_packet && r.ticks > target_ticks && key.is_some() {
                    kept.push(r);
                    break;
                }
                if primary_packet && r.keyframe && r.ticks <= target_ticks {
                    kept.clear();
                    key = Some(r.ticks);
                }
                if key.is_some() {
                    kept.push(r);
                }
            }
            if let Some(k) = key {
                self.out = kept.into_iter().chain(self.out.drain(..)).collect();
                return Ok(self.ticks_to_time(k));
            }
            if start == 0 {
                self.reposition(0, zero);
                return Ok(Duration::ZERO);
            }
            margin = margin.max(CLOCK) * 3;
        }
    }
}

/// 188 or 192 (`.m2ts`) when three packets in a row start with the sync byte.
fn packet_size(head: &[u8]) -> Option<usize> {
    [(TS_PACKET, 0), (192, 4)]
        .into_iter()
        .find(|&(size, skip)| (0..3).all(|k| head.get(skip + k * size) == Some(&0x47)))
        .map(|(size, _)| size)
}

/// Whether `head` (the start of a file or segment) is an MPEG transport stream.
pub(crate) fn is_ts(head: &[u8]) -> bool {
    packet_size(head).is_some()
}

/// The body of a PSI section (after the 8-byte header, without the CRC) with `table_id`.
fn section(payload: &[u8], table_id: u8) -> Option<&[u8]> {
    let pointer = *payload.first()? as usize;
    let t = payload.get(1 + pointer..)?;
    if *t.first()? != table_id {
        return None;
    }
    let len = (u16::from_be_bytes([*t.get(1)?, *t.get(2)?]) & 0x0FFF) as usize;
    t.get(8..(3 + len).checked_sub(4)?)
}

fn descriptor_tags(mut d: &[u8]) -> impl Iterator<Item = u8> + '_ {
    std::iter::from_fn(move || {
        let (&tag, rest) = d.split_first()?;
        let (&len, rest) = rest.split_first()?;
        d = rest.get(len as usize..).unwrap_or_default();
        Some(tag)
    })
}

/// A PES packet's PTS (33-bit, if present) and payload.
fn parse_pes(pes: &[u8]) -> Option<(Option<u64>, &[u8])> {
    if pes.get(..3)? != [0, 0, 1] {
        return None;
    }
    let len = u16::from_be_bytes([*pes.get(4)?, *pes.get(5)?]) as usize;
    let end = if len == 0 { pes.len() } else { (6 + len).min(pes.len()) };
    if matches!(pes[3], 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF) {
        return Some((None, pes.get(6..end)?));
    }
    let flags = *pes.get(7)?;
    let start = 9 + *pes.get(8)? as usize;
    let pts = if flags & 0x80 != 0 {
        let b = pes.get(9..14)?;
        Some(
            ((b[0] as u64 >> 1) & 7) << 30
                | (b[1] as u64) << 22
                | (b[2] as u64 >> 1) << 15
                | (b[3] as u64) << 7
                | b[4] as u64 >> 1,
        )
    } else {
        None
    };
    Some((pts, pes.get(start..end.max(start))?))
}

/// AC-3 / E-AC-3 sample rate and channels from the first sync frame.
fn ac3_info(frame: &[u8], eac3: bool) -> Option<StreamInfo> {
    let b = frame.get(..7)?;
    if b[0] != 0x0B || b[1] != 0x77 {
        return None;
    }
    const RATES: [u32; 3] = [48_000, 44_100, 32_000];
    const CHANNELS: [u16; 8] = [2, 1, 2, 3, 3, 4, 4, 5];
    let (rate, channels) = if eac3 {
        let fscod = b[4] >> 6;
        let rate = if fscod == 3 { [24_000, 22_050, 16_000].get((b[4] >> 4 & 3) as usize).copied()? } else { RATES[fscod as usize] };
        (rate, CHANNELS[(b[4] >> 1 & 7) as usize] + (b[4] & 1) as u16)
    } else {
        (*RATES.get((b[4] >> 6) as usize)?, CHANNELS[(b[6] >> 5) as usize])
    };
    let mut info = StreamInfo::new(0, StreamKind::Audio, if eac3 { Codec::Eac3 } else { Codec::Ac3 });
    info.sample_rate = rate;
    info.channels = channels;
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 188-byte TS packet; the payload is padded with adaptation-field stuffing.
    fn ts_packet(pid: u16, start: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![0x47, (start as u8) << 6 | (pid >> 8) as u8, pid as u8];
        let stuffing = 184 - payload.len();
        if stuffing == 0 {
            p.push(0x10 | cc);
        } else {
            p.push(0x30 | cc);
            p.push(stuffing as u8 - 1);
            if stuffing > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            }
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), 188);
        p
    }

    /// PAT (program 1 → PMT pid 0x1000) and a PMT with the given (stream_type, pid) streams.
    fn psi(streams: &[(u8, u16)]) -> Vec<u8> {
        let pat = [0, 0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, 0xF0, 0x00, 0, 0, 0, 0];
        let mut pmt = vec![0, 0x02, 0xB0, 0, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0];
        for &(ty, pid) in streams {
            pmt.extend_from_slice(&[ty, 0xE0 | (pid >> 8) as u8, pid as u8, 0xF0, 0]);
        }
        pmt.extend_from_slice(&[0; 4]); // CRC (not checked)
        pmt[3] = (pmt.len() - 4) as u8;
        [ts_packet(0, true, 0, &pat), ts_packet(0x1000, true, 0, &pmt)].concat()
    }

    /// A PES packet with a PTS (90 kHz) around `payload`.
    fn pes(stream_id: u8, pts: u64, payload: &[u8], bounded: bool) -> Vec<u8> {
        let len = if bounded { (payload.len() + 8) as u16 } else { 0 };
        let mut p = vec![0, 0, 1, stream_id, (len >> 8) as u8, len as u8, 0x80, 0x80, 5];
        p.extend_from_slice(&[
            0x21 | ((pts >> 29) & 0x0E) as u8,
            (pts >> 22) as u8,
            0x01 | ((pts >> 14) & 0xFE) as u8,
            (pts >> 7) as u8,
            0x01 | ((pts << 1) & 0xFE) as u8,
        ]);
        p.extend_from_slice(payload);
        p
    }

    /// Splits a PES into TS packets on `pid`, continuity counters from `cc`.
    fn packetize(pid: u16, cc: &mut u8, pes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, chunk) in pes.chunks(184).enumerate() {
            out.extend(ts_packet(pid, i == 0, *cc, chunk));
            *cc = (*cc + 1) & 15;
        }
        out
    }

    /// An ADTS frame (AAC-LC, 48 kHz, mono) with `body` as its raw data.
    fn adts(body: &[u8]) -> Vec<u8> {
        let len = body.len() + 7;
        let mut h = vec![0xFF, 0xF1, 0x4C, 0x40 | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1F, 0xFC];
        h.extend_from_slice(body);
        h
    }

    fn segment(data: Vec<u8>) -> TsDemuxer {
        TsDemuxer::open_segment(data, None).unwrap()
    }

    #[test]
    fn a_pes_split_across_packets_is_reassembled() {
        let body: Vec<u8> = (0..300).map(|i| i as u8).collect();
        let frame = adts(&body);
        let mut cc = 0;
        let mut data = psi(&[(0x0F, 0x101)]);
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 90_000, &frame, true)));
        let mut d = segment(data);
        assert_eq!(d.streams().len(), 1);
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.data, body, "ADTS header stripped, payload intact across 3 TS packets");
        assert_eq!(p.pts, Duration::from_secs(1));
        assert!(d.next_packet().unwrap().is_none());
    }

    #[test]
    fn two_adts_frames_in_one_pes_are_two_packets() {
        let mut cc = 0;
        let mut data = psi(&[(0x0F, 0x101)]);
        let both = [adts(&[1; 20]), adts(&[2; 30])].concat();
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 9000, &both, true)));
        let mut d = segment(data);
        let a = d.next_packet().unwrap().unwrap();
        let b = d.next_packet().unwrap().unwrap();
        assert_eq!((a.data.len(), b.data.len()), (20, 30));
        assert_eq!(b.pts - a.pts, Duration::from_secs_f64(1024.0 / 48_000.0));
    }

    #[test]
    fn unbounded_pes_ends_at_the_next_start() {
        let mut cc = 0;
        let mut data = psi(&[(0x0F, 0x101)]);
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 0, &adts(&[1; 10]), false)));
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 1920, &adts(&[2; 10]), false)));
        let mut d = segment(data);
        assert_eq!(d.next_packet().unwrap().unwrap().data, [1; 10]);
        assert_eq!(d.next_packet().unwrap().unwrap().data, [2; 10], "the last one ends with the data");
        assert!(d.next_packet().unwrap().is_none());
    }

    #[test]
    fn a_continuity_gap_drops_the_broken_pes_only() {
        let mut cc = 0;
        let mut data = psi(&[(0x0F, 0x101)]);
        let broken = packetize(0x101, &mut cc, &pes(0xC0, 0, &adts(&[1; 300]), true));
        data.extend_from_slice(&broken[..188]); // its second packet is lost
        data.extend_from_slice(&broken[376..]);
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 1920, &adts(&[2; 10]), true)));
        let mut d = segment(data);
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.data, [2; 10]);
        assert!(d.next_packet().unwrap().is_none());
    }

    #[test]
    fn timestamps_unwrap_past_33_bits() {
        const WRAP: u64 = 1 << 33;
        let mut cc = 0;
        let mut data = psi(&[(0x0F, 0x101)]);
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, WRAP - 1920, &adts(&[1; 10]), true)));
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 0, &adts(&[2; 10]), true)));
        let mut d = segment(data);
        let a = d.next_packet().unwrap().unwrap();
        let b = d.next_packet().unwrap().unwrap();
        assert_eq!(b.pts - a.pts, Duration::from_secs_f64(1920.0 / 90_000.0));
        assert_eq!(d.last_raw_pts(), Some(WRAP));
        // The next segment continues from the reference.
        let mut cc = 0;
        let mut data = psi(&[(0x0F, 0x101)]);
        data.extend(packetize(0x101, &mut cc, &pes(0xC0, 1920, &adts(&[3; 10]), true)));
        let mut d = TsDemuxer::open_segment(data, Some(WRAP)).unwrap();
        let c = d.next_packet().unwrap().unwrap().pts;
        assert!(c.abs_diff(b.pts + Duration::from_secs_f64(1920.0 / 90_000.0)) < Duration::from_micros(1), "{c:?} after {:?}", b.pts);
    }

    #[test]
    fn h264_access_units_become_length_prefixed_with_an_avcc() {
        let sps = [0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfe, 0xab, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x42, 0x99, 0x60];
        let pps = [0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
        let es = [&[0, 0, 0, 1, 0x09, 0xF0][..], &[0, 0, 0, 1], &sps, &[0, 0, 1], &pps, &[0, 0, 1, 0x65, 0x88, 0x80]].concat();
        let mut cc = 0;
        let mut data = psi(&[(0x1B, 0x100)]);
        data.extend(packetize(0x100, &mut cc, &pes(0xE0, 3000, &es, false)));
        data.extend(packetize(0x100, &mut cc, &pes(0xE0, 6600, &[0, 0, 0, 1, 0x41, 0x9a], false)));
        let mut d = segment(data);
        let s = &d.streams()[0];
        assert_eq!((s.codec.clone(), s.width, s.height), (Codec::H264, 318, 238));
        assert_eq!(s.extradata.as_deref(), Some(&crate::nal::avcc_from(&sps, &pps)[..]));
        let key = d.next_packet().unwrap().unwrap();
        assert!(key.keyframe);
        let expected: Vec<u8> = [&sps[..], &pps, &[0x65, 0x88, 0x80]]
            .iter()
            .flat_map(|n| [&(n.len() as u32).to_be_bytes()[..], n].concat())
            .collect();
        assert_eq!(key.data, expected, "AUD dropped, parameter sets kept in-band");
        let next = d.next_packet().unwrap().unwrap();
        assert!(!next.keyframe);
        assert_eq!(next.data, [0, 0, 0, 2, 0x41, 0x9a]);
    }
}
