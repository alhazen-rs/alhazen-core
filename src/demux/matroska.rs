//! WebM / Matroska demuxer with Cues-based keyframe seeking.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::ebml::{EbmlReader, Header, id};
use super::metadata::{CoverPick, Field, MAX_PICTURE, MAX_TEXT};
use super::{Codec, Demuxer, Metadata, Packet, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

const TRACK_TYPE_VIDEO: u64 = 1;
const TRACK_TYPE_AUDIO: u64 = 2;

pub struct MatroskaDemuxer {
    r: EbmlReader,
    streams: Vec<StreamInfo>,
    scale_ns: u64,
    segment_start: u64,
    /// Absolute end of the Segment, when its size is known. EOF before it means truncation.
    segment_end: Option<u64>,
    first_cluster: u64,
    /// (cue time in timestamp ticks, absolute cluster offset), sorted by time.
    cues: Vec<(u64, u64)>,
    cluster_ts: u64,
    /// After a seek, drop video packets until the first keyframe.
    need_keyframe: bool,
    video_track: Option<u32>,
    /// Frames split out of a laced block, returned before reading further.
    pending: VecDeque<Packet>,
    /// Per-track DefaultDuration, used to timestamp laced frames after the first.
    default_durations: HashMap<u32, Duration>,
    metadata: Option<Metadata>,
}

/// Where SeekHead says the elements that may follow the clusters are (relative to the segment).
#[derive(Default)]
struct SeekTargets {
    cues: Option<u64>,
    tags: Option<u64>,
    attachments: Option<u64>,
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
            segment_end: segment.size.map(|size| segment.data_start + size),
            first_cluster: 0,
            cues: Vec::new(),
            cluster_ts: 0,
            need_keyframe: false,
            video_track: None,
            pending: VecDeque::new(),
            default_durations: HashMap::new(),
            metadata: None,
        };
        let mut seek = SeekTargets::default();
        let mut duration_ticks = None;
        let (mut meta, mut covers, mut segment_title) = (Metadata::default(), CoverPick::default(), None);
        let (mut have_tags, mut have_attachments) = (false, false);
        loop {
            let start = d.r.position();
            let h = d.r.read_header()?.ok_or_else(|| demux("no Cluster found"))?;
            match h.id {
                id::SEEK_HEAD => {
                    let s = d.parse_seek_head(h)?;
                    seek.cues = s.cues.or(seek.cues);
                    seek.tags = s.tags.or(seek.tags);
                    seek.attachments = s.attachments.or(seek.attachments);
                }
                id::INFO => duration_ticks = d.parse_info(h, &mut segment_title)?,
                // Tags are optional: a damaged element is skipped, never fatal.
                id::TAGS => {
                    if let Err(e) = d.parse_tags(h, &mut meta) {
                        log::warn!("matroska: tags not read: {e}");
                        d.r.seek_to(h.data_start + known(h)?)?;
                    }
                    have_tags = true;
                }
                id::ATTACHMENTS => {
                    if let Err(e) = d.parse_attachments(h, &mut covers) {
                        log::warn!("matroska: attachments not read: {e}");
                        d.r.seek_to(h.data_start + known(h)?)?;
                    }
                    have_attachments = true;
                }
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
            && let Some(rel) = seek.cues
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
        if d.r.is_seekable() {
            // Tags and attachments written after the clusters (ffmpeg puts its Tags there).
            let mut moved = false;
            for (rel, want, done) in [(seek.tags, id::TAGS, have_tags), (seek.attachments, id::ATTACHMENTS, have_attachments)] {
                let Some(rel) = rel.filter(|_| !done) else { continue };
                d.r.seek_to(d.segment_start + rel)?;
                moved = true;
                let read = match d.r.read_header() {
                    Ok(Some(h)) if h.id == want && want == id::TAGS => d.parse_tags(h, &mut meta),
                    Ok(Some(h)) if h.id == want => d.parse_attachments(h, &mut covers),
                    Ok(_) => Ok(()),
                    Err(e) => Err(e.into()),
                };
                if let Err(e) = read {
                    log::warn!("matroska: tags or attachments not read: {e}");
                }
            }
            if moved {
                d.r.seek_to(d.first_cluster)?;
            }
        }
        // Tags win over the segment title.
        if let Some(title) = segment_title {
            meta.set(Field::Title, &title);
        }
        covers.finish(&mut meta);
        d.metadata = (!meta.is_empty()).then_some(meta);
        Ok(d)
    }

    fn parse_seek_head(&mut self, h: Header) -> Result<SeekTargets> {
        let mut found = SeekTargets::default();
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
            match target.map(|t| t as u32) {
                Some(id::CUES) => found.cues = pos,
                Some(id::TAGS) => found.tags = pos,
                Some(id::ATTACHMENTS) => found.attachments = pos,
                _ => {}
            }
        }
        Ok(found)
    }

    /// `Tags`: every `SimpleTag` name/value at the top level of each `Tag` (targets are ignored).
    fn parse_tags(&mut self, h: Header, meta: &mut Metadata) -> Result<()> {
        let end = h.data_start + known(h)?;
        while self.r.position() < end {
            let tag = self.header()?;
            if tag.id != id::TAG {
                self.r.skip(known(tag)?)?;
                continue;
            }
            let tag_end = tag.data_start + known(tag)?;
            while self.r.position() < tag_end {
                let c = self.header()?;
                if c.id != id::SIMPLE_TAG {
                    self.r.skip(known(c)?)?;
                    continue;
                }
                let simple_end = c.data_start + known(c)?;
                let (mut name, mut value) = (None, None);
                while self.r.position() < simple_end {
                    let e = self.header()?;
                    let size = known(e)?;
                    match e.id {
                        id::TAG_NAME if size <= 256 => name = Some(self.r.read_string(size)?),
                        id::TAG_STRING if size as usize <= MAX_TEXT => value = Some(self.r.read_string(size)?),
                        _ => self.r.skip(size)?, // nested SimpleTags, binary values
                    }
                }
                if let (Some(name), Some(value)) = (name, value) {
                    let field = match name.to_ascii_uppercase().as_str() {
                        "TITLE" => Field::Title,
                        "ARTIST" => Field::Artist,
                        "ALBUM" => Field::Album,
                        "ALBUM_ARTIST" | "ALBUMARTIST" => Field::AlbumArtist,
                        "PART_NUMBER" | "TRACKNUMBER" | "TRACK" => Field::Track,
                        "DATE" | "DATE_RELEASED" | "DATE_RECORDED" | "YEAR" => Field::Year,
                        "GENRE" => Field::Genre,
                        _ => continue,
                    };
                    meta.set(field, &value);
                }
            }
        }
        Ok(())
    }

    /// `Attachments`: images; one named `cover.*` is the front cover.
    fn parse_attachments(&mut self, h: Header, covers: &mut CoverPick) -> Result<()> {
        let end = h.data_start + known(h)?;
        while self.r.position() < end {
            let f = self.header()?;
            if f.id != id::ATTACHED_FILE {
                self.r.skip(known(f)?)?;
                continue;
            }
            let file_end = f.data_start + known(f)?;
            let (mut name, mut mime, mut data) = (String::new(), String::new(), None);
            while self.r.position() < file_end {
                let c = self.header()?;
                let size = known(c)?;
                match c.id {
                    id::FILE_NAME if size <= 1024 => name = self.r.read_string(size)?,
                    id::FILE_MIME_TYPE if size <= 256 => mime = self.r.read_string(size)?,
                    // Only images: other attachments (often fonts, tens of MB) are skipped unread.
                    // FileName and FileMediaType come before FileData.
                    id::FILE_DATA
                        if size as usize <= MAX_PICTURE
                            && (mime.starts_with("image/") || name.to_ascii_lowercase().starts_with("cover")) =>
                    {
                        data = Some(self.r.read_bytes(size)?)
                    }
                    _ => self.r.skip(size)?,
                }
            }
            let lower = name.to_ascii_lowercase();
            if let Some(data) = data
                && (mime.starts_with("image/") || lower.starts_with("cover"))
            {
                covers.offer(lower.starts_with("cover.") || lower.starts_with("cover_"), &mime, &data);
            }
        }
        Ok(())
    }

    fn parse_info(&mut self, h: Header, title: &mut Option<String>) -> Result<Option<f64>> {
        let mut duration = None;
        let end = h.data_start + known(h)?;
        while self.r.position() < end {
            let c = self.header()?;
            match c.id {
                id::TIMESTAMP_SCALE => self.scale_ns = self.r.read_uint(known(c)?)?.max(1),
                id::DURATION => duration = Some(self.r.read_float(known(c)?)?),
                id::TITLE if known(c)? as usize <= MAX_TEXT => *title = Some(self.r.read_string(known(c)?)?),
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
            let mut info = StreamInfo::new(0, StreamKind::Other, Codec::Other(String::new()));
            let mut default_duration = None;
            let mut bit_depth = 0u16;
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
                    id::FLAG_DEFAULT => info.default = self.r.read_uint(size)? != 0,
                    id::CODEC_DELAY => info.codec_delay = Duration::from_nanos(self.r.read_uint(size)?),
                    id::SEEK_PRE_ROLL => info.seek_preroll = Duration::from_nanos(self.r.read_uint(size)?),
                    id::DEFAULT_DURATION => default_duration = Some(Duration::from_nanos(self.r.read_uint(size)?)),
                    id::AUDIO => {
                        let audio_end = c.data_start + size;
                        while self.r.position() < audio_end {
                            let a = self.header()?;
                            match a.id {
                                id::SAMPLING_FREQUENCY => info.sample_rate = self.r.read_float(known(a)?)? as u32,
                                id::CHANNELS => info.channels = self.r.read_uint(known(a)?)? as u16,
                                id::BIT_DEPTH => bit_depth = self.r.read_uint(known(a)?)? as u16,
                                _ => self.r.skip(known(a)?)?,
                            }
                        }
                    }
                    id::VIDEO => {
                        let video_end = c.data_start + size;
                        while self.r.position() < video_end {
                            let v = self.header()?;
                            match v.id {
                                id::PIXEL_WIDTH => info.width = self.r.read_uint(known(v)?)? as u32,
                                id::PIXEL_HEIGHT => info.height = self.r.read_uint(known(v)?)? as u32,
                                id::COLOUR => {
                                    let colour_end = v.data_start + known(v)?;
                                    while self.r.position() < colour_end {
                                        let c = self.header()?;
                                        match c.id {
                                            // 2 = unspecified.
                                            id::MATRIX_COEFFICIENTS => {
                                                let m = self.r.read_uint(known(c)?)?;
                                                info.color_matrix = (m != 2 && m < 256).then_some(m as u8);
                                            }
                                            // 0 unspecified, 1 broadcast, 2 full, 3 defined by matrix.
                                            id::RANGE => match self.r.read_uint(known(c)?)? {
                                                1 => info.full_range = Some(false),
                                                2 => info.full_range = Some(true),
                                                _ => {}
                                            },
                                            _ => self.r.skip(known(c)?)?,
                                        }
                                    }
                                }
                                _ => self.r.skip(known(v)?)?,
                            }
                        }
                    }
                    _ => self.r.skip(size)?,
                }
            }
            if info.id != 0 {
                if info.kind == StreamKind::Audio && info.channels == 0 {
                    info.channels = 1; // Matroska default
                }
                if let Codec::Pcm(f) = &mut info.codec {
                    f.bits = bit_depth;
                    // Matroska: "8-bit PCM is unsigned" (A_PCM/INT/LIT).
                    f.signed = f.float || bit_depth != 8;
                }
                if let Some(d) = default_duration {
                    self.default_durations.insert(info.id, d);
                }
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

    /// Parses a (Simple)Block into packets (several when laced), queued in `pending`.
    fn queue_block(&mut self, data: Vec<u8>, keyframe_hint: Option<bool>) {
        let Some((track, n)) = slice_vint(&data) else { return };
        let header_len = n + 3;
        if data.len() < header_len {
            return;
        }
        let rel = i16::from_be_bytes([data[n], data[n + 1]]);
        let flags = data[n + 2];
        let track = track as u32;
        if !self.streams.iter().any(|s| s.id == track) {
            return;
        }
        let keyframe = keyframe_hint.unwrap_or(flags & 0x80 != 0);
        if self.need_keyframe && Some(track) == self.video_track {
            if !keyframe {
                return;
            }
            self.need_keyframe = false;
        }
        let pts = self.ticks_to_duration(self.cluster_ts as i64 + rel as i64);
        let lacing = (flags >> 1) & 0b11;
        if lacing == 0 {
            let mut payload = data;
            payload.drain(..header_len);
            self.pending.push_back(Packet { stream: track, pts, keyframe, data: payload, generation: 0 });
            return;
        }
        let Some(frames) = split_laced(lacing, &data[header_len..]) else {
            log::warn!("dropping malformed laced block on track {track}");
            return;
        };
        let step = self.default_durations.get(&track).copied().unwrap_or_default();
        for (i, frame) in frames.into_iter().enumerate() {
            self.pending.push_back(Packet {
                stream: track,
                pts: pts + step * i as u32,
                keyframe,
                data: frame.to_vec(),
                generation: 0,
            });
        }
    }
}

impl Demuxer for MatroskaDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.pending.pop_front() {
                return Ok(Some(p));
            }
            let Some(h) = self.r.read_header()? else {
                if let Some(end) = self.segment_end
                    && self.r.position() < end
                {
                    return Err(demux("file ends before the end of its Segment (truncated?)"));
                }
                return Ok(None);
            };
            match h.id {
                // Descend into clusters without skipping them.
                id::CLUSTER => {}
                id::TIMESTAMP => self.cluster_ts = self.r.read_uint(known(h)?)?,
                id::SIMPLE_BLOCK => {
                    let data = self.r.read_bytes(known(h)?)?;
                    self.queue_block(data, None);
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
                    if let Some(data) = block {
                        self.queue_block(data, Some(!has_reference));
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
        self.pending.clear();
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
            let end = h.data_start + size;
            // A cluster is a valid seek point only if it holds a video keyframe at or before the
            // target; otherwise `need_keyframe` would skip forward past the target.
            if let Some(key_ts) = self.first_video_keyframe(ts, end, ticks)? {
                best = (start, key_ts);
            }
            self.r.seek_to(end)?;
        }
        Ok(best)
    }

    /// Timestamp of the first video keyframe in the cluster (children up to `end`, cluster
    /// timestamp `cluster_ts`) if it is at or before `ticks`.
    fn first_video_keyframe(&mut self, cluster_ts: u64, end: u64, ticks: u64) -> Result<Option<u64>> {
        let Some(video) = self.video_track else {
            return Ok(Some(cluster_ts));
        };
        // (track, relative timestamp) from the first bytes of a (Simple)Block.
        let block_head = |head: &[u8]| {
            let (track, n) = slice_vint(head)?;
            let rel = i16::from_be_bytes([*head.get(n)?, *head.get(n + 1)?]);
            Some((track as u32, rel, head.get(n + 2).copied()))
        };
        while self.r.position() < end {
            let h = self.header()?;
            let size = known(h)?;
            let (block, keyframe) = match h.id {
                id::SIMPLE_BLOCK => {
                    let head = self.r.read_bytes(size.min(11))?;
                    self.r.skip(size - head.len() as u64)?;
                    let block = block_head(&head);
                    let key = block.and_then(|(_, _, flags)| flags).is_some_and(|f| f & 0x80 != 0);
                    (block, key)
                }
                id::BLOCK_GROUP => {
                    let group_end = h.data_start + size;
                    let (mut block, mut has_reference) = (None, false);
                    while self.r.position() < group_end {
                        let c = self.header()?;
                        let c_size = known(c)?;
                        match c.id {
                            id::BLOCK => {
                                let head = self.r.read_bytes(c_size.min(11))?;
                                self.r.skip(c_size - head.len() as u64)?;
                                block = block_head(&head);
                            }
                            id::REFERENCE_BLOCK => {
                                has_reference = true;
                                self.r.skip(c_size)?;
                            }
                            _ => self.r.skip(c_size)?,
                        }
                    }
                    (block, !has_reference)
                }
                _ => {
                    self.r.skip(size)?;
                    continue;
                }
            };
            if let Some((track, rel, _)) = block
                && track == video
                && keyframe
            {
                let ts = (cluster_ts as i64 + rel as i64).max(0) as u64;
                return Ok((ts <= ticks).then_some(ts));
            }
        }
        Ok(None)
    }
}

fn known(h: Header) -> Result<u64> {
    h.size.ok_or_else(|| demux(&format!("unexpected unknown-size element {:#X}", h.id)))
}

fn demux(msg: &str) -> Error {
    Error::Demux(msg.to_owned())
}

/// Splits Xiph-laced data (frame count byte, sizes, frames), e.g. Vorbis CodecPrivate.
#[cfg(feature = "native")]
pub(crate) fn split_xiph_lacing(data: &[u8]) -> Option<Vec<&[u8]>> {
    split_laced(1, data)
}

/// Splits a laced block payload (starting at the frame-count byte) into frames.
/// `lacing`: 1 = Xiph, 2 = fixed-size, 3 = EBML.
fn split_laced(lacing: u8, payload: &[u8]) -> Option<Vec<&[u8]>> {
    let count = *payload.first()? as usize + 1;
    let mut pos = 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 0..count - 1 {
                let mut size = 0usize;
                loop {
                    let b = *payload.get(pos)?;
                    pos += 1;
                    size += b as usize;
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        2 => {
            let rest = payload.len() - pos;
            if !rest.is_multiple_of(count) {
                return None;
            }
            sizes = vec![rest / count; count - 1];
        }
        3 => {
            let (first, n) = slice_vint(payload.get(pos..)?)?;
            pos += n;
            let mut size = first as i64;
            sizes.push(size as usize);
            for _ in 1..count - 1 {
                let (raw, n) = slice_vint(payload.get(pos..)?)?;
                pos += n;
                let bias = (1i64 << (7 * n - 1)) - 1;
                size += raw as i64 - bias;
                if size < 0 {
                    return None;
                }
                sizes.push(size as usize);
            }
        }
        _ => return None,
    }
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        frames.push(payload.get(pos..pos + size)?);
        pos += size;
    }
    frames.push(payload.get(pos..)?);
    Some(frames)
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
    fn seek_without_cues_never_lands_after_target_when_gop_spans_clusters() {
        // Clusters every ~250 ms, keyframes only at 0 and 1000 ms.
        let mut d = open("tests/fixtures/av1_small_clusters.webm");
        d.cues.clear();
        for (target_ms, expect_ms) in [(900, 0), (1500, 1000), (400, 0)] {
            let landed = d.seek(Duration::from_millis(target_ms)).unwrap();
            let p = d.next_packet().unwrap().unwrap();
            assert!(p.keyframe);
            assert_eq!(p.pts, Duration::from_millis(expect_ms), "seek to {target_ms}ms");
            assert!(landed <= p.pts, "reported position must not be after the first packet");
        }
    }

    #[test]
    fn truncation_at_element_boundary_is_an_error() {
        let bytes = std::fs::read("tests/fixtures/av1.webm").unwrap();
        let mut d = open("tests/fixtures/av1.webm");
        for _ in 0..40 {
            d.next_packet().unwrap().unwrap();
        }
        let cut = d.r.position() as usize;
        assert!(cut < bytes.len());
        let path = std::env::temp_dir().join(format!("vc-boundary-cut-{}.webm", std::process::id()));
        std::fs::write(&path, &bytes[..cut]).unwrap();
        let mut d = MatroskaDemuxer::open(Box::new(FileSource::open(&path).unwrap())).unwrap();
        let mut result = Ok(Some(()));
        while let Ok(Some(_)) = result {
            result = d.next_packet().map(|p| p.map(|_| ()));
        }
        std::fs::remove_file(&path).ok();
        assert!(result.is_err(), "file cut at a block boundary must be an error, got {result:?}");
    }

    #[test]
    fn reads_audio_track_fields() {
        let d = open("tests/fixtures/opus_only.webm");
        let a = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap();
        assert_eq!(a.codec, Codec::Opus);
        assert_eq!((a.sample_rate, a.channels), (48_000, 1));
        assert_eq!(a.codec_delay, Duration::from_nanos(6_500_000));
        assert_eq!(a.seek_preroll, Duration::from_millis(80));
        assert!(!a.default, "ffmpeg writes FlagDefault=0; parsed value must override the default of true");
        let d = open("tests/fixtures/av1_vorbis.webm");
        let a = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap();
        assert_eq!((a.codec.clone(), a.sample_rate, a.channels), (Codec::Vorbis, 44_100, 1));
        assert!(a.extradata.as_ref().is_some_and(|x| x.len() > 100), "Vorbis headers in CodecPrivate");
    }

    #[test]
    fn audio_packets_are_delivered() {
        let mut d = open("tests/fixtures/av1_with_audio.webm");
        let audio = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().id;
        let mut n = 0;
        let mut last = Duration::ZERO;
        while let Some(p) = d.next_packet().unwrap() {
            if p.stream == audio {
                assert!(p.pts >= last);
                last = p.pts;
                n += 1;
            }
        }
        assert!(n >= 99, "2 s of 20 ms Opus packets, got {n}");
    }

    #[test]
    fn splits_xiph_fixed_and_ebml_lacing() {
        // 3 frames of sizes 2, 300, 1 (Xiph: 300 = 255 + 45).
        let mut xiph = vec![2, 2, 255, 45];
        xiph.extend([1u8; 2]);
        xiph.extend([2u8; 300]);
        xiph.extend([3u8; 1]);
        let f = split_laced(1, &xiph).unwrap();
        assert_eq!(f.iter().map(|x| x.len()).collect::<Vec<_>>(), [2, 300, 1]);
        assert_eq!((f[0][0], f[1][0], f[2][0]), (1, 2, 3));
        // Fixed: 3 frames of 4 bytes.
        let mut fixed = vec![2];
        fixed.extend([9u8; 12]);
        assert_eq!(split_laced(2, &fixed).unwrap().iter().map(|x| x.len()).collect::<Vec<_>>(), [4, 4, 4]);
        assert!(split_laced(2, &[2, 1, 2, 3, 4]).is_none(), "12 bytes not divisible by 3 -> None");
        // EBML: sizes 10, 7 (diff -3), last = rest (5). First size vint 0x8A, diff -3 as 1-byte signed vint: 0x80 | (63-3).
        let mut ebml = vec![2, 0x8A, 0x80 | 60];
        ebml.extend([0u8; 10 + 7 + 5]);
        assert_eq!(split_laced(3, &ebml).unwrap().iter().map(|x| x.len()).collect::<Vec<_>>(), [10, 7, 5]);
        assert!(split_laced(1, &[5, 1]).is_none(), "truncated sizes -> None");
    }

    #[test]
    fn rejects_non_matroska() {
        let src = Box::new(FileSource::open("tests/fixtures/not_video.bin").unwrap());
        assert!(MatroskaDemuxer::open(src).is_err());
    }

    #[test]
    fn laced_file_yields_the_same_packets_as_unlaced() {
        // laced_vorbis.webm is vorbis_only.webm with every 3 blocks Xiph-laced (make_laced.py).
        let payloads = |path: &str| {
            let mut d = open(path);
            let mut out = vec![];
            while let Some(p) = d.next_packet().unwrap() {
                out.push(p.data);
            }
            out
        };
        let (plain, laced) = (payloads("tests/fixtures/vorbis_only.webm"), payloads("tests/fixtures/laced_vorbis.webm"));
        assert_eq!(laced.len(), plain.len());
        assert!(laced == plain, "laced frames must split back into the original packets");
    }

    #[test]
    fn reads_container_colour_info() {
        let src = Box::new(FileSource::open("tests/fixtures/mjpeg_full_range.mkv").unwrap());
        let d = MatroskaDemuxer::open(src).unwrap();
        assert_eq!(d.streams()[0].full_range, Some(true), "JPEG video is tagged full range");
        let src = Box::new(FileSource::open("tests/fixtures/av1.webm").unwrap());
        let d = MatroskaDemuxer::open(src).unwrap();
        // ffmpeg tagged the AV1 fixture limited ("tv") range, matrix unspecified.
        assert_eq!((d.streams()[0].color_matrix, d.streams()[0].full_range), (None, Some(false)));
    }

    #[test]
    fn maps_windows_audio_codecs() {
        for (name, codec) in [("mp3.mkv", Codec::Mp3), ("ac3.mkv", Codec::Ac3), ("eac3.mkv", Codec::Eac3), ("flac.mkv", Codec::Flac)] {
            let d = MatroskaDemuxer::open(Box::new(FileSource::open(format!("tests/fixtures/{name}")).unwrap())).unwrap();
            let s = &d.streams()[0];
            assert_eq!((s.kind, &s.codec, s.sample_rate, s.channels), (StreamKind::Audio, &codec, 48_000, 2), "{name}");
        }
    }
}
