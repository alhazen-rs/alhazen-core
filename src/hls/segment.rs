//! One HLS segment's media: MPEG-TS, fragmented MP4 (with its init section) or packed audio.
//!
//! Packets come out with **raw** times: the TS clock (unwrapped), the fMP4 `tfdt` time, or the
//! packed-audio ID3 timestamp plus the frame's offset. The HLS demuxer maps them onto the
//! playlist's timeline.

use std::collections::VecDeque;
use std::time::Duration;

use crate::demux::{self, Codec, ContainerFormat, Demuxer, StreamInfo, StreamKind};
use crate::source::{MediaSource, MemorySource};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegmentFormat {
    Ts,
    Fmp4,
    PackedAudio,
}

/// What a segment's first bytes say it is.
pub(crate) fn detect(data: &[u8]) -> Option<SegmentFormat> {
    if demux::ts_is_ts(data) {
        return Some(SegmentFormat::Ts);
    }
    if data.len() >= 8 && matches!(&data[4..8], b"ftyp" | b"styp" | b"moof" | b"moov" | b"sidx" | b"free" | b"prft" | b"emsg") {
        return Some(SegmentFormat::Fmp4);
    }
    let mut src = MemorySource::new(data[..data.len().min(256 * 1024)].to_vec(), "segment");
    match demux::probe(&mut src).ok().flatten() {
        Some(ContainerFormat::Adts | ContainerFormat::Mp3) => Some(SegmentFormat::PackedAudio),
        _ => None,
    }
}

/// 90 kHz ticks as a `Duration` (rounded down to the nanosecond, as the TS demuxer does).
pub(crate) fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_nanos((ticks as u128 * 1_000_000_000 / 90_000) as u64)
}

/// A packet of the segment's first video or first audio stream, with its raw time.
#[derive(Clone, Debug)]
pub(crate) struct RawPacket {
    pub kind: StreamKind,
    pub raw: Duration,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

enum Inner {
    Ts(Box<demux::TsDemuxer>),
    Other(Box<dyn Demuxer>),
}

impl Inner {
    fn get(&mut self) -> &mut dyn Demuxer {
        match self {
            Inner::Ts(d) => d.as_mut(),
            Inner::Other(d) => d.as_mut(),
        }
    }
}

pub(crate) struct SegmentDemuxer {
    /// The first video and first audio stream (ids are the inner demuxer's).
    pub streams: Vec<StreamInfo>,
    inner: Inner,
    /// Packed audio: the ID3 timestamp added to the reader's times.
    offset: Duration,
    update: Option<StreamInfo>,
    /// Packets read ahead by `first_raw`, with the format update each came with.
    peeked: VecDeque<(RawPacket, Option<StreamInfo>)>,
}

impl SegmentDemuxer {
    /// `init`: the fMP4 initialization section (`EXT-X-MAP`); `ts_reference`: the previous TS
    /// segment's last timestamp, for unwrapping.
    pub fn open(data: Vec<u8>, init: Option<&[u8]>, ts_reference: Option<u64>) -> Result<Self> {
        Self::open_with(data, init, ts_reference, None)
    }

    /// `params`: the previous TS segment's video parameter sets.
    pub fn open_with(data: Vec<u8>, init: Option<&[u8]>, ts_reference: Option<u64>, params: Option<demux::VideoParams>) -> Result<Self> {
        let format = detect(&data).or_else(|| init.map(|_| SegmentFormat::Fmp4));
        let (inner, offset) = match format {
            Some(SegmentFormat::Ts) => (Inner::Ts(Box::new(demux::TsDemuxer::open_segment_with(data, ts_reference, params)?)), Duration::ZERO),
            Some(SegmentFormat::Fmp4) => {
                let bytes = match init {
                    Some(init) => [init, &data].concat(),
                    None => data,
                };
                (Inner::Other(Box::new(demux::Mp4Demuxer::open(Box::new(MemorySource::new(bytes, "fMP4 segment")))?)), Duration::ZERO)
            }
            Some(SegmentFormat::PackedAudio) => {
                let offset = packed_audio_timestamp(&data).unwrap_or_default();
                let mut src: Box<dyn MediaSource> = Box::new(MemorySource::new(data, "packed audio segment"));
                let d: Box<dyn Demuxer> = match demux::probe(src.as_mut())? {
                    Some(ContainerFormat::Mp3) => Box::new(demux::Mp3Demuxer::open(src)?),
                    _ => Box::new(demux::AdtsDemuxer::open(src)?),
                };
                (Inner::Other(d), offset)
            }
            None => return Err(Error::Demux("HLS segment of unknown format".into())),
        };
        let mut d = Self { streams: Vec::new(), inner, offset, update: None, peeked: VecDeque::new() };
        let all = d.inner.get().streams().to_vec();
        for kind in [StreamKind::Video, StreamKind::Audio] {
            if let Some(s) = all.iter().find(|s| s.kind == kind && !matches!(s.codec, Codec::Other(_))) {
                d.streams.push(s.clone());
            }
        }
        if d.streams.is_empty() {
            return Err(Error::Demux("HLS segment without audio or video".into()));
        }
        Ok(d)
    }

    /// The earliest timestamp among the first packets of each stream (what the segment's start
    /// on the timeline corresponds to).
    pub fn first_raw(&mut self) -> Result<Option<Duration>> {
        const LOOK_AHEAD: usize = 64;
        while self.peeked.len() < LOOK_AHEAD
            && !self.streams.iter().all(|s| self.peeked.iter().any(|(p, _)| p.kind == s.kind))
        {
            match self.read()? {
                Some(p) => {
                    let u = self.update.take();
                    self.peeked.push_back((p, u));
                }
                None => break,
            }
        }
        let mut firsts = self.streams.iter().filter_map(|s| self.peeked.iter().find(|(p, _)| p.kind == s.kind).map(|(p, _)| p.raw));
        Ok(firsts.by_ref().min())
    }

    pub fn next(&mut self) -> Result<Option<RawPacket>> {
        if let Some((p, u)) = self.peeked.pop_front() {
            self.update = u;
            return Ok(Some(p));
        }
        self.read()
    }

    fn read(&mut self) -> Result<Option<RawPacket>> {
        loop {
            let Some(p) = self.inner.get().next_packet()? else { return Ok(None) };
            let update = self.inner.get().take_stream_update();
            let Some(i) = self.streams.iter().position(|s| s.id == p.stream) else { continue };
            if let Some(info) = update.filter(|u| u.id == p.stream) {
                self.streams[i] = info.clone();
                self.update = Some(info);
            }
            return Ok(Some(RawPacket { kind: self.streams[i].kind, raw: p.pts + self.offset, keyframe: p.keyframe, data: p.data }));
        }
    }

    /// A stream whose format changed with the packet just returned.
    pub fn take_update(&mut self) -> Option<StreamInfo> {
        self.update.take()
    }

    /// TS only: the video parameter sets, for the next segment.
    pub fn ts_video_params(&self) -> Option<demux::VideoParams> {
        match &self.inner {
            Inner::Ts(d) => d.video_params(),
            Inner::Other(_) => None,
        }
    }

    /// TS only: the highest timestamp seen, to unwrap the next segment's.
    pub fn ts_last_raw(&self) -> Option<u64> {
        match &self.inner {
            Inner::Ts(d) => d.last_raw_pts(),
            Inner::Other(_) => None,
        }
    }
}

/// The ID3 `PRIV` timestamp (`com.apple.streaming.transportStreamTimestamp`) that starts a packed
/// audio segment.
pub(crate) fn packed_audio_timestamp(data: &[u8]) -> Option<Duration> {
    const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp\0";
    if data.get(..3)? != b"ID3" {
        return None;
    }
    let version = data[3];
    let syncsafe = |b: &[u8]| b.iter().fold(0usize, |n, &x| n << 7 | (x & 0x7F) as usize);
    let size = syncsafe(data.get(6..10)?);
    let tag = data.get(10..10 + size)?;
    let mut pos = 0;
    if data[5] & 0x40 != 0 {
        // Extended header.
        let len = tag.get(..4)?;
        pos = if version >= 4 { syncsafe(len) } else { u32::from_be_bytes(len.try_into().ok()?) as usize + 4 };
    }
    while pos + 10 <= tag.len() {
        let id = &tag[pos..pos + 4];
        let len_bytes = &tag[pos + 4..pos + 8];
        let len = if version >= 4 { syncsafe(len_bytes) } else { u32::from_be_bytes(len_bytes.try_into().ok()?) as usize };
        let body = tag.get(pos + 10..pos + 10 + len)?;
        if id == b"PRIV"
            && let Some(ts) = body.strip_prefix(OWNER)
        {
            let ticks = u64::from_be_bytes(ts.get(..8)?.try_into().ok()?) & ((1 << 33) - 1);
            return Some(ticks_to_duration(ticks));
        }
        if id[0] == 0 {
            break; // padding
        }
        pos += 10 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/hls/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn all(mut d: SegmentDemuxer) -> Vec<RawPacket> {
        std::iter::from_fn(|| d.next().unwrap()).collect()
    }

    #[test]
    fn detects_formats() {
        let ts = std::fs::read(format!("{}/tests/fixtures/h264_aac.ts", env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert_eq!(detect(&ts), Some(SegmentFormat::Ts));
        assert_eq!(detect(&fixture("fmp4/seg0.m4s")), Some(SegmentFormat::Fmp4), "styp first");
        assert_eq!(detect(&fixture("fmp4/init.mp4")), Some(SegmentFormat::Fmp4));
        assert_eq!(detect(&fixture("packed/seg0.aac")), Some(SegmentFormat::PackedAudio));
        assert_eq!(detect(b"<html>not media</html>"), None);
    }

    #[test]
    fn fmp4_segment_gives_hevc_and_aac_timed_by_tfdt() {
        let init = fixture("fmp4/init.mp4");
        let d = SegmentDemuxer::open(fixture("fmp4/seg1.m4s"), Some(&init), None).unwrap();
        let video = d.streams.iter().find(|s| s.kind == StreamKind::Video).unwrap();
        assert_eq!((video.codec.clone(), video.width, video.height), (Codec::Hevc, 320, 180));
        assert!(video.extradata.is_some(), "hvcC");
        let audio = d.streams.iter().find(|s| s.kind == StreamKind::Audio).unwrap();
        assert_eq!((audio.codec.clone(), audio.sample_rate), (Codec::Aac, 48_000));
        let packets = all(d);
        let first_video = packets.iter().find(|p| p.kind == StreamKind::Video).unwrap();
        assert!(first_video.keyframe);
        // x265 put this keyframe at 1.96 s; the video edit list (2 frames of B-frame delay) is
        // applied, so it is presented at 1.88 s.
        assert!(first_video.raw.abs_diff(Duration::from_millis(1880)) < Duration::from_millis(5), "{:?}", first_video.raw);
        assert!(packets.iter().filter(|p| p.kind == StreamKind::Video).count() >= 50);
    }

    #[test]
    fn packed_audio_is_timed_by_its_priv_timestamp() {
        let data = fixture("packed/seg1.aac");
        let start = packed_audio_timestamp(&data).unwrap();
        assert_eq!(start, ticks_to_duration(1_080_480), "900000 + 94 frames of 1024 at 48 kHz");
        let d = SegmentDemuxer::open(data, None, None).unwrap();
        assert_eq!(d.streams.len(), 1);
        assert_eq!(d.streams[0].codec, Codec::Aac);
        let packets = all(d);
        assert_eq!(packets[0].raw, start);
        assert_eq!(packets.len(), 94);
        assert!(packets.iter().all(|p| p.kind == StreamKind::Audio && p.keyframe));
    }

    #[test]
    fn ts_segments_keep_raw_unwrapped_times() {
        let ts = std::fs::read(format!("{}/tests/fixtures/h264_aac.ts", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut d = SegmentDemuxer::open(ts, None, None).unwrap();
        let first = d.next().unwrap().unwrap();
        assert!(first.raw > Duration::from_secs(1), "raw TS clock (ffmpeg starts at 1.4 s): {:?}", first.raw);
        while d.next().unwrap().is_some() {}
        assert!(d.ts_last_raw().unwrap() > 90_000 * 4, "{:?}", d.ts_last_raw());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(SegmentDemuxer::open(b"<html>404</html>".to_vec(), None, None).is_err());
    }
}
