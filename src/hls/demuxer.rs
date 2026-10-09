//! `HlsDemuxer`: plays an HLS presentation through the ordinary `Demuxer` interface. The main
//! playlist (a variant) and, when the variant's audio is separate, an audio rendition are fetched
//! by one `Track` each; their packets are mapped onto one timeline and returned in time order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use url::Url;

use super::abr::Abr;
use super::http::Fetcher;
use super::playlist::{self, Playlist, Rendition, VariantStream};
use super::segment::SegmentDemuxer;
use super::timeline::{Role, Timeline};
use super::track::{SegmentData, Start, Track, TrackConfig, TrackEvent};
use crate::backend::Registry;
use crate::demux::{Codec, Demuxer, Packet, StreamInfo, StreamKind};
use crate::{Error, Result};

const VIDEO_ID: u32 = 1;
const AUDIO_ID: u32 = 2;
const POLL: Duration = Duration::from_millis(50);

/// One variant of an HLS presentation, as offered to the user.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantInfo {
    /// Peak bits per second.
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Vec<String>,
    pub frame_rate: Option<f64>,
}

/// Which variant to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Adaptive bitrate: chosen from measured throughput.
    Auto,
    /// This one (an index into `variants()`), until `Auto` is chosen again.
    Index(usize),
}

/// An alternative audio track of an HLS presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRendition {
    pub name: String,
    pub language: Option<String>,
}

/// What the application can see and steer while an HLS demuxer runs (shared with the player).
pub struct HlsControl {
    variants: Vec<VariantInfo>,
    renditions: Vec<AudioRendition>,
    current: AtomicUsize,
    requested: Mutex<Option<Variant>>,
    /// The player is going away: stop waiting for downloads.
    shutdown: AtomicBool,
}

impl HlsControl {
    pub fn variants(&self) -> Vec<VariantInfo> {
        self.variants.clone()
    }

    /// The variant being played (index into `variants()`).
    pub fn current_variant(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }

    pub fn set_variant(&self, v: Variant) {
        *self.requested.lock().unwrap() = Some(v);
    }

    pub fn audio_renditions(&self) -> Vec<AudioRendition> {
        self.renditions.clone()
    }

    pub(crate) fn shut_down(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// A packet ready to be returned.
struct Out {
    id: u32,
    pts: Duration,
    keyframe: bool,
    data: Vec<u8>,
    info: Arc<StreamInfo>,
}

/// The segment a lane is reading.
struct Open {
    demux: SegmentDemuxer,
    offset: i128,
    variant: usize,
    disc: u64,
}

/// One playlist's packets on their way out.
struct Lane {
    role: Role,
    track: Track,
    seg: Option<Open>,
    pending: Option<Out>,
    /// Current format of each kind of stream in this lane.
    formats: HashMap<StreamKind, Arc<StreamInfo>>,
    /// TS timestamp unwrapping reference, and the discontinuity sequence it belongs to.
    ts_ref: Option<(u64, u64)>,
    ended: bool,
    /// Seeks sent to the track: older segments are dropped.
    epoch: u64,
    /// Main lane with a separate audio rendition: its own audio is dropped.
    use_audio: bool,
    /// After a seek: drop packets until a video keyframe.
    wait_key: bool,
}

impl Lane {
    fn new(role: Role, track: Track, use_audio: bool) -> Self {
        Self { role, track, seg: None, pending: None, formats: HashMap::new(), ts_ref: None, ended: false, epoch: 0, use_audio, wait_key: false }
    }
}

pub struct HlsDemuxer {
    control: Arc<HlsControl>,
    main: Lane,
    audio: Option<Lane>,
    streams: Vec<StreamInfo>,
    timeline: Timeline,
    update: Option<StreamInfo>,
    /// The format last announced for each id.
    reported: HashMap<u32, Arc<StreamInfo>>,
    live: bool,
    duration: Option<Duration>,
    abr: Abr,
    blacklist: Vec<bool>,
}

impl HlsDemuxer {
    /// Opens the playlist at `url` (master or media) and the first segments. `registry` decides
    /// which variants are playable.
    pub fn open(url: &Url, registry: &Registry) -> Result<HlsDemuxer> {
        let cancel = Arc::new(AtomicBool::new(false));
        let fetch = Fetcher::new(cancel.clone());
        let first = fetch.get(url, None)?;
        let text = String::from_utf8_lossy(&first.data);
        let (variants, renditions) = match playlist::parse(&text, &first.url)? {
            Playlist::Media(_) => {
                let only = VariantStream {
                    uri: first.url.clone(),
                    bandwidth: 0,
                    average_bandwidth: None,
                    codecs: Vec::new(),
                    resolution: None,
                    frame_rate: None,
                    audio_group: None,
                };
                (vec![only], Vec::new())
            }
            Playlist::Master(m) => (m.variants, m.audio),
        };
        let mut blacklist: Vec<bool> = variants.iter().map(|v| !decodable(registry, &v.codecs)).collect();
        if blacklist.iter().all(|&b| b) {
            let mut codecs: Vec<String> = variants.iter().flat_map(|v| v.codecs.clone()).collect();
            codecs.sort();
            codecs.dedup();
            return Err(Error::UnsupportedCodec { codec: codecs.join(", "), tried_backends: registry.names() });
        }
        let bitrates: Vec<u64> = variants.iter().map(|v| v.bandwidth).collect();
        let playlists: Vec<Url> = variants.iter().map(|v| v.uri.clone()).collect();
        // The first variant whose playlist is playable (DRM-protected ones are left out).
        let (main, chosen) = loop {
            let chosen = Abr::initial(&bitrates, first.data.len() as u64, first.elapsed, &blacklist);
            let cfg = TrackConfig { playlists: playlists.clone(), variant: chosen, start: Start::LiveEdge, cancel: cancel.clone() };
            match Track::start(cfg) {
                Ok(t) => break (t, chosen),
                Err(e @ Error::Unsupported(_)) => {
                    blacklist[chosen] = true;
                    if blacklist.iter().all(|&b| b) {
                        return Err(e);
                    }
                    log::warn!("HLS: variant {chosen} cannot be played ({e}); trying another");
                }
                Err(e) => return Err(e),
            }
        };
        let rendition = pick_rendition(&renditions, variants[chosen].audio_group.as_deref());
        let audio = match rendition.and_then(|r| r.uri.clone()) {
            Some(uri) => {
                let cfg = TrackConfig { playlists: vec![uri], variant: 0, start: Start::LiveEdge, cancel: cancel.clone() };
                Some(Lane::new(Role::Audio, Track::start(cfg)?, true))
            }
            None => None,
        };
        let live = main.is_live();
        let duration = main.duration();
        let tolerance = Duration::from_secs(1).max(main.target_duration() * 2);
        let control = Arc::new(HlsControl {
            variants: variants
                .iter()
                .map(|v| VariantInfo { bandwidth: v.bandwidth, resolution: v.resolution, codecs: v.codecs.clone(), frame_rate: v.frame_rate })
                .collect(),
            renditions: renditions.iter().map(|r| AudioRendition { name: r.name.clone(), language: r.language.clone() }).collect(),
            current: AtomicUsize::new(chosen),
            requested: Mutex::new(None),
            shutdown: AtomicBool::new(false),
        });
        let mut d = HlsDemuxer {
            control,
            main: Lane::new(Role::Main, main, audio.is_none()),
            audio,
            streams: Vec::new(),
            timeline: Timeline::new(tolerance),
            update: None,
            reported: HashMap::new(),
            live,
            duration,
            abr: Abr::new(bitrates),
            blacklist,
        };
        d.fill_all()?;
        let video = d.main.formats.get(&StreamKind::Video).cloned();
        let audio = match &d.audio {
            Some(a) => a.formats.get(&StreamKind::Audio).cloned(),
            None => d.main.formats.get(&StreamKind::Audio).cloned(),
        };
        for (id, info) in [(VIDEO_ID, video), (AUDIO_ID, audio)] {
            if let Some(info) = info {
                d.reported.insert(id, info.clone());
                let mut s = (*info).clone();
                s.id = id;
                s.duration = duration;
                d.streams.push(s);
            }
        }
        if d.streams.is_empty() {
            return Err(Error::Demux("HLS stream without audio or video".into()));
        }
        Ok(d)
    }

    /// Steering and information for the application.
    pub fn control(&self) -> Arc<HlsControl> {
        self.control.clone()
    }

    /// Live streams have no duration and cannot seek.
    pub fn is_live(&self) -> bool {
        self.live
    }

    fn fill_all(&mut self) -> Result<()> {
        Self::fill(&mut self.main, &mut self.timeline, &self.control)?;
        if let Some(a) = self.audio.as_mut() {
            Self::fill(a, &mut self.timeline, &self.control)?;
        }
        Ok(())
    }

    /// Makes `lane.pending` hold the lane's next packet, unless the lane has ended.
    fn fill(lane: &mut Lane, timeline: &mut Timeline, control: &HlsControl) -> Result<()> {
        while lane.pending.is_none() && !lane.ended {
            if control.shutdown.load(Ordering::Relaxed) {
                lane.ended = true;
                break;
            }
            if let Some(open) = lane.seg.as_mut() {
                let Some(raw) = open.demux.next()? else {
                    // The next segment unwraps its TS timestamps from where this one ended.
                    if let Some(r) = open.demux.ts_last_raw() {
                        lane.ts_ref = Some((r, open.disc));
                    }
                    lane.seg = None;
                    continue;
                };
                if let Some(u) = open.demux.take_update() {
                    lane.formats.insert(u.kind, Arc::new(u));
                }
                let wanted = match raw.kind {
                    StreamKind::Video => lane.role == Role::Main,
                    StreamKind::Audio => lane.use_audio,
                    StreamKind::Other => false,
                };
                if !wanted {
                    continue;
                }
                if lane.wait_key {
                    if raw.kind != StreamKind::Video || !raw.keyframe {
                        continue;
                    }
                    lane.wait_key = false;
                }
                let (id, info) = match raw.kind {
                    StreamKind::Video => (VIDEO_ID, lane.formats[&StreamKind::Video].clone()),
                    _ => (AUDIO_ID, lane.formats[&StreamKind::Audio].clone()),
                };
                lane.pending = Some(Out { id, pts: Timeline::map(open.offset, raw.raw), keyframe: raw.keyframe, data: raw.data, info });
                continue;
            }
            match lane.track.recv(POLL) {
                None => {}
                Some(TrackEvent::Segment(s)) if s.epoch < lane.epoch => {}
                Some(TrackEvent::Segment(s)) => Self::open_segment(lane, timeline, s)?,
                Some(TrackEvent::End) => lane.ended = true,
                Some(TrackEvent::Failed(e)) => return Err(e),
            }
        }
        Ok(())
    }

    fn open_segment(lane: &mut Lane, timeline: &mut Timeline, s: SegmentData) -> Result<()> {
        let reference = lane.ts_ref.filter(|&(_, disc)| disc == s.discontinuity_seq).map(|(r, _)| r);
        let mut demux = match SegmentDemuxer::open(s.data, s.init.as_deref().map(Vec::as_slice), reference) {
            Ok(d) => d,
            Err(e) if lane.track.is_live() => {
                log::warn!("HLS: skipping segment {}: {e}", s.seq);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for info in &demux.streams {
            lane.formats.insert(info.kind, Arc::new(info.clone()));
        }
        let first = demux.first_raw()?.unwrap_or_default();
        let offset = timeline.anchor(lane.role, s.discontinuity_seq, s.start, first);
        lane.seg = Some(Open { demux, offset, variant: s.variant, disc: s.discontinuity_seq });
        Ok(())
    }
}

impl Demuxer for HlsDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.fill_all()?;
        let take_audio = match (&self.main.pending, self.audio.as_ref().and_then(|a| a.pending.as_ref())) {
            (None, None) => return Ok(None),
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some(m), Some(a)) => a.pts <= m.pts,
        };
        let out = if take_audio { self.audio.as_mut().unwrap().pending.take() } else { self.main.pending.take() }.unwrap();
        let changed = self.reported.get(&out.id).is_none_or(|r| !same_format(r, &out.info));
        if changed {
            self.reported.insert(out.id, out.info.clone());
            let mut info = (*out.info).clone();
            info.id = out.id;
            info.duration = self.duration;
            if let Some(s) = self.streams.iter_mut().find(|s| s.id == out.id) {
                *s = info.clone();
            }
            self.update = Some(info);
        }
        Ok(Some(Packet { stream: out.id, pts: out.pts, keyframe: out.keyframe, data: out.data, generation: 0 }))
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        if self.live {
            return Err(Error::Seek("live HLS streams cannot seek".into()));
        }
        let has_video = self.streams.iter().any(|s| s.kind == StreamKind::Video);
        for lane in std::iter::once(&mut self.main).chain(self.audio.as_mut()) {
            lane.track.seek(target);
            lane.epoch += 1;
            lane.seg = None;
            lane.pending = None;
            lane.ended = false;
            lane.ts_ref = None;
        }
        self.main.wait_key = has_video;
        self.fill_all()?;
        Ok(self.main.pending.as_ref().or(self.audio.as_ref().and_then(|a| a.pending.as_ref())).map_or(target, |p| p.pts))
    }

    fn take_stream_update(&mut self) -> Option<StreamInfo> {
        self.update.take()
    }
}

/// Whether a decoder for `a` can go on with `b`.
fn same_format(a: &StreamInfo, b: &StreamInfo) -> bool {
    a.codec == b.codec
        && a.extradata == b.extradata
        && (a.width, a.height, a.sample_rate, a.channels) == (b.width, b.height, b.sample_rate, b.channels)
}

/// Whether every codec named in a variant's `CODECS` can be decoded (unknown names pass: the
/// segment will tell).
fn decodable(registry: &Registry, codecs: &[String]) -> bool {
    codecs.iter().all(|c| {
        let codec = Codec::from_mp4_codec_string(c);
        let kind = match codec {
            Codec::Av1 | Codec::Vp8 | Codec::Vp9 | Codec::H264 | Codec::Hevc | Codec::ProRes => StreamKind::Video,
            Codec::Other(_) => return true,
            _ => StreamKind::Audio,
        };
        registry.can_decode(&StreamInfo::new(0, kind, codec))
    })
}

/// The audio rendition of `group` to play: the default one with its own playlist, else the
/// first with one. `None` when the variant's audio is muxed into its own segments.
fn pick_rendition<'a>(renditions: &'a [Rendition], group: Option<&str>) -> Option<&'a Rendition> {
    let group = group?;
    let mut in_group = renditions.iter().filter(|r| r.group == group && r.uri.is_some());
    let all: Vec<&Rendition> = in_group.by_ref().collect();
    all.iter().find(|r| r.default).or(all.first()).copied()
}
