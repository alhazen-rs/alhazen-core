//! WebM / Matroska demuxer with Cues-based keyframe seeking.

use std::time::Duration;

use super::ebml::{EbmlReader, Header, id};
use super::{Codec, Demuxer, Packet, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

const TRACK_TYPE_VIDEO: u64 = 1;
const TRACK_TYPE_AUDIO: u64 = 2;

pub struct MatroskaDemuxer {
    r: EbmlReader,
    streams: Vec<StreamInfo>,
    scale_ns: u64,
    segment_start: u64,
    first_cluster: u64,
    /// (cue time in timestamp ticks, absolute cluster offset), sorted by time.
    cues: Vec<(u64, u64)>,
    cluster_ts: u64,
    /// After a seek, drop video packets until the first keyframe.
    need_keyframe: bool,
    video_track: Option<u32>,
}

impl MatroskaDemuxer {
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut r = EbmlReader::new(src);
        let ebml = r.read_header()?.ok_or_else(|| demux("empty file"))?;
        if ebml.id != id::EBML {
            return Err(Error::UnsupportedContainer);
        }
        r.skip(known(ebml)?)?;
        let segment = r.read_header()?.ok_or_else(|| demux("missing Segment"))?;
        if segment.id != id::SEGMENT {
            return Err(demux("missing Segment"));
        }
        let mut d = MatroskaDemuxer {
            r,
            streams: Vec::new(),
            scale_ns: 1_000_000,
            segment_start: segment.data_start,
            first_cluster: 0,
            cues: Vec::new(),
            cluster_ts: 0,
            need_keyframe: false,
            video_track: None,
        };
        let mut cues_pos = None;
        let mut duration_ticks = None;
        loop {
            let start = d.r.position();
            let h = d.r.read_header()?.ok_or_else(|| demux("no Cluster found"))?;
            match h.id {
                id::SEEK_HEAD => cues_pos = d.parse_seek_head(h)?.or(cues_pos),
                id::INFO => duration_ticks = d.parse_info(h)?,
                id::TRACKS => d.parse_tracks(h)?,
                id::CUES => d.parse_cues(h)?,
                id::CLUSTER => {
                    d.first_cluster = start;
                    break;
                }
                _ => d.r.skip(known(h)?)?,
            }
        }
        if d.streams.is_empty() {
            return Err(demux("no tracks"));
        }
        let duration = duration_ticks.map(|t| Duration::from_nanos((t * d.scale_ns as f64) as u64));
        for s in &mut d.streams {
            s.duration = duration;
        }
        d.video_track = d.streams.iter().find(|s| s.kind == StreamKind::Video).map(|s| s.id);
        if d.cues.is_empty()
            && d.r.is_seekable()
            && let Some(rel) = cues_pos
        {
            // Cues usually sit after the clusters; read them, then come back.
            d.r.seek_to(d.segment_start + rel)?;
            if let Some(h) = d.r.read_header()?
                && h.id == id::CUES
            {
                d.parse_cues(h)?;
            }
            d.r.seek_to(d.first_cluster)?;
        }
        Ok(d)
    }

    fn parse_seek_head(&mut self, h: Header) -> Result<Option<u64>> {
        let mut cues_pos = None;
        let end = h.data_start + known(h)?;
        while self.r.position() < end {
            let seek = self.header()?;
            if seek.id != id::SEEK {
                self.r.skip(known(seek)?)?;
                continue;
            }
            let seek_end = seek.data_start + known(seek)?;
            let (mut target, mut pos) = (None, None);
            while self.r.position() < seek_end {
                let c = self.header()?;
                match c.id {
                    id::SEEK_ID => target = Some(self.r.read_uint(known(c)?)?),
                    id::SEEK_POSITION => pos = Some(self.r.read_uint(known(c)?)?),
                    _ => self.r.skip(known(c)?)?,
                }
            }
            if target == Some(id::CUES as u64) {
                cues_pos = pos;
            }
        }
        Ok(cues_pos)
    }

    fn parse_info(&mut self, h: Header) -> Result<Option<f64>> {
        let mut duration = None;
        let end = h.data_start + known(h)?;
        while self.r.position() < end {
            let c = self.header()?;
            match c.id {
                id::TIMESTAMP_SCALE => self.scale_ns = self.r.read_uint(known(c)?)?.max(1),
                id::DURATION => duration = Some(self.r.read_float(known(c)?)?),
                _ => self.r.skip(known(c)?)?,
            }
        }
        Ok(duration)
    }

    fn parse_tracks(&mut self, h: Header) -> Result<()> {
        let end = h.data_start + known(h)?;
        while self.r.position() < end {
            let entry = self.header()?;
            if entry.id != id::TRACK_ENTRY {
                self.r.skip(known(entry)?)?;
                continue;
            }
            let entry_end = entry.data_start + known(entry)?;
            let mut info = StreamInfo {
                id: 0,
                kind: StreamKind::Other,
                codec: Codec::Other(String::new()),
                width: 0,
                height: 0,
                duration: None,
                extradata: None,
            };
            while self.r.position() < entry_end {
                let c = self.header()?;
                let size = known(c)?;
                match c.id {
                    id::TRACK_NUMBER => info.id = self.r.read_uint(size)? as u32,
                    id::TRACK_TYPE => {
                        info.kind = match self.r.read_uint(size)? {
                            TRACK_TYPE_VIDEO => StreamKind::Video,
                            TRACK_TYPE_AUDIO => StreamKind::Audio,
                            _ => StreamKind::Other,
                        }
                    }
                    id::CODEC_ID => info.codec = Codec::from_matroska_id(&self.r.read_string(size)?),
                    id::CODEC_PRIVATE => info.extradata = Some(self.r.read_bytes(size)?),
                    id::VIDEO => {
                        let video_end = c.data_start + size;
                        while self.r.position() < video_end {
                            let v = self.header()?;
                            match v.id {
                                id::PIXEL_WIDTH => info.width = self.r.read_uint(known(v)?)? as u32,
                                id::PIXEL_HEIGHT => info.height = self.r.read_uint(known(v)?)? as u32,
                                _ => self.r.skip(known(v)?)?,
                            }
                        }
                    }
                    _ => self.r.skip(size)?,
                }
            }
            if info.id != 0 {
                self.streams.push(info);
            }
        }
        Ok(())
    }

    fn parse_cues(&mut self, h: Header) -> Result<()> {
        let end = h.data_start + known(h)?;
        let video = self.video_track.map(u64::from);
        while self.r.position() < end {
            let point = self.header()?;
            if point.id != id::CUE_POINT {
                self.r.skip(known(point)?)?;
                continue;
            }
            let point_end = point.data_start + known(point)?;
            let mut time = None;
            let mut positions = Vec::new();
            while self.r.position() < point_end {
                let c = self.header()?;
                match c.id {
                    id::CUE_TIME => time = Some(self.r.read_uint(known(c)?)?),
                    id::CUE_TRACK_POSITIONS => {
                        let tp_end = c.data_start + known(c)?;
                        let (mut track, mut pos) = (None, None);
                        while self.r.position() < tp_end {
                            let t = self.header()?;
                            match t.id {
                                id::CUE_TRACK => track = Some(self.r.read_uint(known(t)?)?),
                                id::CUE_CLUSTER_POSITION => pos = Some(self.r.read_uint(known(t)?)?),
                                _ => self.r.skip(known(t)?)?,
                            }
                        }
                        if let Some(pos) = pos
                            && (video.is_none() || track == video)
                        {
                            positions.push(pos);
                        }
                    }
                    _ => self.r.skip(known(c)?)?,
                }
            }
            if let (Some(time), Some(pos)) = (time, positions.first()) {
                self.cues.push((time, self.segment_start + pos));
            }
        }
        self.cues.sort_unstable();
        Ok(())
    }

    fn header(&mut self) -> Result<Header> {
        self.r.read_header()?.ok_or_else(|| demux("unexpected end of file"))
    }

    fn ticks_to_duration(&self, ticks: i64) -> Duration {
        Duration::from_nanos((ticks.max(0) as u64).saturating_mul(self.scale_ns))
    }

    /// Parses a (Simple)Block payload into a packet; `None` for skipped blocks.
    fn block_to_packet(&mut self, data: Vec<u8>, keyframe_hint: Option<bool>) -> Option<Packet> {
        let (track, n) = slice_vint(&data)?;
        let header_len = n + 3;
        if data.len() < header_len {
            return None;
        }
        let rel = i16::from_be_bytes([data[n], data[n + 1]]);
        let flags = data[n + 2];
        let track = track as u32;
        if !self.streams.iter().any(|s| s.id == track) {
            return None;
        }
        if (flags >> 1) & 0b11 != 0 {
            // Laced blocks are only used for audio; lacing support arrives with audio in phase 2.
            log::debug!("skipping laced block on track {track}");
            return None;
        }
        let keyframe = keyframe_hint.unwrap_or(flags & 0x80 != 0);
        if self.need_keyframe && Some(track) == self.video_track {
            if !keyframe {
                return None;
            }
            self.need_keyframe = false;
        }
        let pts = self.ticks_to_duration(self.cluster_ts as i64 + rel as i64);
        let mut payload = data;
        payload.drain(..header_len);
        Some(Packet { stream: track, pts, keyframe, data: payload, generation: 0 })
    }
}

impl Demuxer for MatroskaDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let Some(h) = self.r.read_header()? else {
                return Ok(None);
            };
            match h.id {
                // Descend into clusters without skipping them.
                id::CLUSTER => {}
                id::TIMESTAMP => self.cluster_ts = self.r.read_uint(known(h)?)?,
                id::SIMPLE_BLOCK => {
                    let data = self.r.read_bytes(known(h)?)?;
                    if let Some(p) = self.block_to_packet(data, None) {
                        return Ok(Some(p));
                    }
                }
                id::BLOCK_GROUP => {
                    let end = h.data_start + known(h)?;
                    let (mut block, mut has_reference) = (None, false);
                    while self.r.position() < end {
                        let c = self.header()?;
                        match c.id {
                            id::BLOCK => block = Some(self.r.read_bytes(known(c)?)?),
                            id::REFERENCE_BLOCK => {
                                has_reference = true;
                                self.r.skip(known(c)?)?;
                            }
                            _ => self.r.skip(known(c)?)?,
                        }
                    }
                    if let Some(data) = block
                        && let Some(p) = self.block_to_packet(data, Some(!has_reference))
                    {
                        return Ok(Some(p));
                    }
                }
                _ => self.r.skip(known(h)?)?,
            }
        }
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        if !self.r.is_seekable() {
            return Err(Error::Seek(format!("{} is not seekable", self.r.source().description())));
        }
        let ticks = (target.as_nanos() / self.scale_ns as u128) as u64;
        let (pos, ts) = match self.cues.iter().rev().find(|(t, _)| *t <= ticks) {
            Some(&(t, pos)) => (pos, t),
            None if !self.cues.is_empty() => (self.cues[0].1, self.cues[0].0),
            None => self.scan_clusters(ticks)?,
        };
        self.r.seek_to(pos)?;
        self.cluster_ts = ts;
        self.need_keyframe = true;
        Ok(self.ticks_to_duration(ts as i64))
    }
}

impl MatroskaDemuxer {
    /// Without Cues: walk cluster headers to find the last cluster starting at or before `ticks`.
    fn scan_clusters(&mut self, ticks: u64) -> Result<(u64, u64)> {
        let mut best = (self.first_cluster, 0);
        self.r.seek_to(self.first_cluster)?;
        loop {
            let start = self.r.position();
            let Some(h) = self.r.read_header()? else { break };
            if h.id != id::CLUSTER {
                match h.size {
                    Some(size) => {
                        self.r.skip(size)?;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(size) = h.size else { break };
            let first = self.header()?;
            if first.id != id::TIMESTAMP {
                break;
            }
            let ts = self.r.read_uint(known(first)?)?;
            if ts > ticks {
                break;
            }
            best = (start, ts);
            self.r.seek_to(h.data_start + size)?;
        }
        Ok(best)
    }
}

fn known(h: Header) -> Result<u64> {
    h.size.ok_or_else(|| demux(&format!("unexpected unknown-size element {:#X}", h.id)))
}

fn demux(msg: &str) -> Error {
    Error::Demux(msg.to_owned())
}

/// EBML size-style vint from a byte slice (marker bit stripped). Returns (value, length).
fn slice_vint(data: &[u8]) -> Option<(u64, usize)> {
    let first = *data.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || data.len() < len {
        return None;
    }
    let mut v = (first as u64) & (0xFF >> len);
    for b in &data[1..len] {
        v = (v << 8) | *b as u64;
    }
    Some((v, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileSource;

    fn open(path: &str) -> MatroskaDemuxer {
        MatroskaDemuxer::open(Box::new(FileSource::open(path).unwrap())).unwrap()
    }

    fn video_packets(d: &mut MatroskaDemuxer) -> Vec<Packet> {
        let mut out = vec![];
        while let Some(p) = d.next_packet().unwrap() {
            if p.stream == 1 {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn reads_stream_info() {
        let d = open("tests/fixtures/av1.webm");
        let s = &d.streams()[0];
        assert_eq!((s.id, s.kind, s.codec.clone()), (1, StreamKind::Video, Codec::Av1));
        assert_eq!((s.width, s.height), (320, 240));
        assert_eq!(s.duration, Some(Duration::from_secs(2)));
    }

    #[test]
    fn reads_all_packets_in_order() {
        let mut d = open("tests/fixtures/av1.webm");
        let pkts = video_packets(&mut d);
        assert_eq!(pkts.len(), 60);
        assert!(pkts[0].keyframe);
        assert_eq!(pkts[0].pts, Duration::ZERO);
        assert!(pkts.windows(2).all(|w| w[0].pts < w[1].pts));
        assert_eq!(pkts.iter().filter(|p| p.keyframe).count(), 2);
    }

    #[test]
    fn audio_track_is_listed() {
        let d = open("tests/fixtures/av1_with_audio.webm");
        assert!(d.streams().iter().any(|s| s.kind == StreamKind::Audio && s.codec == Codec::Opus));
    }

    #[test]
    fn seek_lands_on_keyframe_at_or_before_target() {
        let mut d = open("tests/fixtures/av1.webm");
        for (target_ms, expect_ms) in [(0, 0), (500, 0), (1000, 1000), (1500, 1000), (1990, 1000)] {
            let landed = d.seek(Duration::from_millis(target_ms)).unwrap();
            assert_eq!(landed, Duration::from_millis(expect_ms), "seek to {target_ms}ms");
            let p = d.next_packet().unwrap().unwrap();
            assert!(p.keyframe);
            assert_eq!(p.pts, Duration::from_millis(expect_ms));
        }
    }

    #[test]
    fn seek_without_cues_scans_clusters() {
        let mut d = open("tests/fixtures/av1.webm");
        d.cues.clear();
        let landed = d.seek(Duration::from_millis(1500)).unwrap();
        assert_eq!(landed, Duration::from_millis(1000));
        let p = d.next_packet().unwrap().unwrap();
        assert!(p.keyframe);
        assert_eq!(p.pts, Duration::from_millis(1000));
    }

    #[test]
    fn truncated_file_errors_instead_of_hanging() {
        let mut d = open("tests/fixtures/truncated.webm");
        let mut result = Ok(Some(()));
        while let Ok(Some(_)) = result {
            result = d.next_packet().map(|p| p.map(|_| ()));
        }
        assert!(result.is_err(), "truncated file must surface an error, got {result:?}");
    }

    #[test]
    fn rejects_non_matroska() {
        let src = Box::new(FileSource::open("tests/fixtures/not_video.bin").unwrap());
        assert!(MatroskaDemuxer::open(src).is_err());
    }
}
