//! One media playlist being played: a thread that fetches its segments ahead (with keys and init
//! sections), follows live playlists, and switches variants on request.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};
use url::Url;

use super::crypto::{decrypt_aes128, sequence_iv};
use super::http::Fetcher;
use super::playlist::{self, InitSection, MediaPlaylist, Playlist};
use crate::{Error, Result};

/// Segments fetched ahead of the reader.
const AHEAD: usize = 3;
const POLL: Duration = Duration::from_millis(50);
/// A live playlist that has not grown for this many target durations has ended.
const LIVE_STALL_TARGETS: u32 = 3;
/// Live playback starts this many target durations before the end of the playlist.
const LIVE_JOIN_TARGETS: u32 = 3;

pub(crate) enum Start {
    At(Duration),
    LiveEdge,
}

pub(crate) struct TrackConfig {
    /// One media playlist per variant (a single one for an audio rendition).
    pub playlists: Vec<Url>,
    pub variant: usize,
    pub start: Start,
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum TrackEvent {
    Segment(SegmentData),
    /// The playlist ended (VOD end, or a live stream that stopped). Carries the seek epoch it
    /// belongs to, like segments: an end reached before a seek is stale after it.
    End(u64),
    Failed(Error, u64),
}

pub(crate) struct SegmentData {
    pub seq: u64,
    pub discontinuity_seq: u64,
    /// Where it starts on the playlist timeline (VOD: sum of earlier durations; live: since the
    /// first segment played).
    pub start: Duration,
    pub duration: Duration,
    /// Decrypted.
    pub data: Vec<u8>,
    /// The fMP4 initialization section.
    pub init: Option<Arc<Vec<u8>>>,
    pub variant: usize,
    /// Bytes downloaded and how long it took (for ABR).
    pub bytes: u64,
    pub elapsed: Duration,
    /// Number of seeks before this segment was fetched.
    pub epoch: u64,
}

impl std::fmt::Debug for SegmentData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Segment(seq {}, variant {}, start {:?}, {} bytes, epoch {})", self.seq, self.variant, self.start, self.data.len(), self.epoch)
    }
}

enum Command {
    Seek(Duration),
    Switch(usize),
}

pub(crate) struct Track {
    events: Receiver<TrackEvent>,
    commands: Sender<Command>,
    live: bool,
    duration: Option<Duration>,
    target_duration: Duration,
    cancel: Arc<AtomicBool>,
}

impl Track {
    /// Loads the starting variant's playlist (errors are reported here), then fetches in the
    /// background.
    pub fn start(cfg: TrackConfig) -> Result<Track> {
        let fetch = Fetcher::new(cfg.cancel.clone());
        let variant = cfg.variant.min(cfg.playlists.len().saturating_sub(1));
        let first = load(&fetch, &cfg.playlists[variant])?;
        let live = !first.1.ended;
        let duration = (!live).then(|| first.1.total_duration());
        let target_duration = first.1.target_duration;
        let next = match cfg.start {
            Start::LiveEdge if live => live_edge(&first.1),
            _ => first.1.index_at(start_time(&cfg.start)).map(|i| first.1.segments[i].sequence),
        };
        let mut urls = cfg.playlists.clone();
        urls[variant] = first.0;
        let mut playlists = vec![None; urls.len()];
        playlists[variant] = Some(Arc::new(first.1));
        let (event_tx, events) = crossbeam_channel::bounded(AHEAD);
        let (commands, command_rx) = crossbeam_channel::unbounded();
        let worker = Worker {
            fetch,
            urls,
            playlists,
            variant,
            next,
            next_start: Duration::ZERO,
            live,
            epoch: 0,
            events: event_tx,
            commands: command_rx,
            cancel: cfg.cancel.clone(),
            keys: HashMap::new(),
            inits: HashMap::new(),
            last_reload: Instant::now(),
            last_change: Instant::now(),
            unchanged: false,
        };
        thread::Builder::new().name("hls-fetch".into()).spawn(move || worker.run())?;
        Ok(Track { events, commands, live, duration, target_duration, cancel: cfg.cancel })
    }

    /// The next event, or `None` after `timeout` (or once the thread has stopped).
    /// A fetch thread that stopped without a final event (it panicked) is reported as a failure.
    pub fn recv(&self, timeout: Duration) -> Option<TrackEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(e) => Some(e),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(TrackEvent::Failed(Error::Http("the HLS download thread stopped".into()), u64::MAX)),
        }
    }

    /// Restarts fetching at the segment holding `t` (VOD); events after it carry a new epoch.
    pub fn seek(&self, t: Duration) {
        let _ = self.commands.send(Command::Seek(t));
    }

    /// Continues with `variant`'s segments from the next segment on.
    pub fn switch(&self, variant: usize) {
        let _ = self.commands.send(Command::Switch(variant));
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    /// VOD: the playlist's length.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    pub fn target_duration(&self) -> Duration {
        self.target_duration
    }
}

impl Drop for Track {
    /// The fetch thread is not joined: it notices the cancel flag (or the closed channel) within
    /// one read and exits on its own.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn start_time(start: &Start) -> Duration {
    match start {
        Start::At(t) => *t,
        Start::LiveEdge => Duration::ZERO,
    }
}

/// Fetches and parses a media playlist; returns its final URL too.
fn load(fetch: &Fetcher, url: &Url) -> Result<(Url, MediaPlaylist)> {
    let f = fetch.get(url, None)?;
    let text = String::from_utf8_lossy(&f.data);
    match playlist::parse(&text, &f.url)? {
        Playlist::Media(p) => Ok((f.url, p)),
        Playlist::Master(_) => Err(Error::InvalidSource(format!("{url}: a master playlist where a media playlist was expected"))),
    }
}

/// The sequence number to start a live playlist at: about three target durations from the end.
fn live_edge(p: &MediaPlaylist) -> Option<u64> {
    let want = p.target_duration * LIVE_JOIN_TARGETS;
    let mut total = Duration::ZERO;
    let mut index = p.segments.len();
    while index > 0 && total < want {
        index -= 1;
        total += p.segments[index].duration;
    }
    p.segments.get(index).map(|s| s.sequence)
}

struct Worker {
    fetch: Fetcher,
    urls: Vec<Url>,
    playlists: Vec<Option<Arc<MediaPlaylist>>>,
    variant: usize,
    /// Sequence number of the next segment to fetch (`None`: empty playlist).
    next: Option<u64>,
    /// Live: where the next segment starts on the timeline.
    next_start: Duration,
    live: bool,
    epoch: u64,
    events: Sender<TrackEvent>,
    commands: Receiver<Command>,
    cancel: Arc<AtomicBool>,
    keys: HashMap<Url, [u8; 16]>,
    inits: HashMap<InitSection, Arc<Vec<u8>>>,
    last_reload: Instant,
    /// When the live playlist last grew.
    last_change: Instant,
    /// The last reload brought nothing new (reload sooner).
    unchanged: bool,
}

/// What the loop does after handling one step.
enum Flow {
    Continue,
    Stop,
}

impl Worker {
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn run(mut self) {
        loop {
            if self.stopped() {
                return;
            }
            while let Ok(c) = self.commands.try_recv() {
                self.on_command(c);
            }
            if let Flow::Stop = self.step() {
                return;
            }
        }
    }

    fn playlist(&mut self, variant: usize) -> Result<Arc<MediaPlaylist>> {
        if let Some(p) = &self.playlists[variant] {
            return Ok(p.clone());
        }
        let (url, p) = load(&self.fetch, &self.urls[variant])?;
        self.urls[variant] = url;
        let p = Arc::new(p);
        self.playlists[variant] = Some(p.clone());
        Ok(p)
    }

    fn on_command(&mut self, c: Command) {
        match c {
            Command::Seek(t) => {
                self.epoch += 1;
                if self.live {
                    return;
                }
                if let Ok(p) = self.playlist(self.variant) {
                    self.next = p.index_at(t).map(|i| p.segments[i].sequence);
                }
            }
            Command::Switch(v) if v < self.urls.len() && v != self.variant => {
                let old = self.playlists[self.variant].clone();
                let Ok(new) = self.playlist(v) else {
                    log::warn!("HLS: variant {v}'s playlist cannot be loaded; staying on {}", self.variant);
                    return;
                };
                // Same sequence number when the new playlist has it, else the same time.
                if let (Some(next), Some(old)) = (self.next, old)
                    && !new.segments.iter().any(|s| s.sequence == next)
                    && let Some(i) = old.segments.iter().position(|s| s.sequence == next)
                {
                    self.next = new.index_at(old.start_of(i)).map(|j| new.segments[j].sequence);
                }
                self.variant = v;
            }
            Command::Switch(_) => {}
        }
    }

    /// Sends `e`, giving up (returning `false`) when a command arrives or the track is dropped.
    fn send(&mut self, mut e: TrackEvent) -> bool {
        loop {
            if self.stopped() {
                return false;
            }
            match self.events.send_timeout(e, POLL) {
                Ok(()) => return true,
                Err(SendTimeoutError::Timeout(back)) => {
                    if !self.commands.is_empty() {
                        return false;
                    }
                    e = back;
                }
                Err(SendTimeoutError::Disconnected(_)) => {
                    self.cancel.store(true, Ordering::Relaxed);
                    return false;
                }
            }
        }
    }

    /// Sends a last event (`End`, `Failed`): unlike `send`, a waiting command does not make it
    /// give up, or the reader would wait for an event that never comes.
    fn send_final(&mut self, e: TrackEvent) -> bool {
        let mut e = e;
        loop {
            if self.stopped() {
                return false;
            }
            match self.events.send_timeout(e, POLL) {
                Ok(()) => return true,
                Err(SendTimeoutError::Timeout(back)) => e = back,
                Err(SendTimeoutError::Disconnected(_)) => return false,
            }
        }
    }

    /// Waits up to `d` for a command (handled) or cancellation.
    fn idle(&mut self, d: Duration) {
        match self.commands.recv_timeout(d.min(POLL)) {
            Ok(c) => self.on_command(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => self.cancel.store(true, Ordering::Relaxed),
        }
    }

    fn step(&mut self) -> Flow {
        let p = match self.playlist(self.variant) {
            Ok(p) => p,
            Err(e) => {
                let _ = self.send_final(TrackEvent::Failed(e, self.epoch));
                return Flow::Stop;
            }
        };
        let first = p.segments.first().map(|s| s.sequence);
        let index = self.next.and_then(|n| p.segments.iter().position(|s| s.sequence == n));
        if let Some(i) = index {
            return self.fetch_segment(&p, i);
        }
        if self.live
            && let (Some(next), Some(first)) = (self.next, first)
            && next < first
        {
            log::warn!("HLS: live playback fell behind the playlist window; jumping to the live edge");
            self.next = live_edge(&p);
            return Flow::Continue;
        }
        if self.live && self.next.is_none() {
            self.next = live_edge(&p);
            if self.next.is_some() {
                return Flow::Continue;
            }
        }
        if !self.live || p.ended {
            // The end: wait for a seek (or for the track to be dropped).
            if !self.send_final(TrackEvent::End(self.epoch)) {
                return Flow::Stop;
            }
            while !self.stopped() {
                let epoch = self.epoch;
                self.idle(POLL);
                if self.epoch != epoch {
                    return Flow::Continue;
                }
            }
            return Flow::Stop;
        }
        self.wait_for_live(&p)
    }

    /// Live, nothing new yet: reload the playlist when due; end when it stopped growing.
    fn wait_for_live(&mut self, p: &MediaPlaylist) -> Flow {
        let target = p.target_duration.max(Duration::from_millis(500));
        let due = self.last_reload + if self.unchanged { target / 2 } else { target };
        if Instant::now() < due {
            self.idle(due - Instant::now());
            return Flow::Continue;
        }
        self.last_reload = Instant::now();
        let last = p.segments.last().map(|s| s.sequence);
        match load(&self.fetch, &self.urls[self.variant]) {
            Ok((url, new)) => {
                let grew = new.segments.last().map(|s| s.sequence) > last || new.ended;
                self.unchanged = !grew;
                if grew {
                    self.last_change = Instant::now();
                }
                self.urls[self.variant] = url;
                // Other variants' playlists are stale now; reloaded when switched to.
                for (i, slot) in self.playlists.iter_mut().enumerate() {
                    if i != self.variant {
                        *slot = None;
                    }
                }
                self.playlists[self.variant] = Some(Arc::new(new));
            }
            Err(e) => {
                log::warn!("HLS: live playlist reload failed: {e}");
                self.unchanged = true;
            }
        }
        if self.last_change.elapsed() > target * LIVE_STALL_TARGETS {
            log::info!("HLS: the live playlist stopped updating; treating the stream as ended");
            if self.send_final(TrackEvent::End(self.epoch)) {
                while !self.stopped() {
                    self.idle(POLL);
                }
            }
            return Flow::Stop;
        }
        Flow::Continue
    }

    fn fetch_segment(&mut self, p: &MediaPlaylist, i: usize) -> Flow {
        let seg = &p.segments[i];
        let start = if self.live { self.next_start } else { p.start_of(i) };
        match self.download(seg) {
            Ok((data, init, bytes, elapsed)) => {
                let event = TrackEvent::Segment(SegmentData {
                    seq: seg.sequence,
                    discontinuity_seq: seg.discontinuity_seq,
                    start,
                    duration: seg.duration,
                    data,
                    init,
                    variant: self.variant,
                    bytes,
                    elapsed,
                    epoch: self.epoch,
                });
                if self.send(event) {
                    self.next = Some(seg.sequence + 1);
                    self.next_start = start + seg.duration;
                }
                Flow::Continue
            }
            Err(_) if self.stopped() => Flow::Stop,
            Err(e) if self.live => {
                log::warn!("HLS: skipping live segment {}: {e}", seg.sequence);
                self.next = Some(seg.sequence + 1);
                self.next_start = start + seg.duration;
                Flow::Continue
            }
            Err(e) => {
                let _ = self.send_final(TrackEvent::Failed(e, self.epoch));
                Flow::Stop
            }
        }
    }

    /// The segment's bytes (decrypted), its init section, and download size/time.
    #[allow(clippy::type_complexity)]
    fn download(&mut self, seg: &playlist::Segment) -> Result<(Vec<u8>, Option<Arc<Vec<u8>>>, u64, Duration)> {
        let init = match &seg.map {
            Some(map) => match self.inits.get(map) {
                Some(i) => Some(i.clone()),
                None => {
                    let data = Arc::new(self.fetch.get(&map.uri, map.byte_range)?.data);
                    self.inits.insert(map.clone(), data.clone());
                    Some(data)
                }
            },
            None => None,
        };
        let f = self.fetch.get(&seg.uri, seg.byte_range)?;
        let bytes = f.data.len() as u64;
        let data = match &seg.key {
            Some(key) => {
                let k = match self.keys.get(&key.uri) {
                    Some(k) => *k,
                    None => {
                        let raw = self.fetch.get(&key.uri, None)?.data;
                        let k: [u8; 16] = raw
                            .as_slice()
                            .try_into()
                            .map_err(|_| Error::Http(format!("{}: an AES-128 key must be 16 bytes, got {}", key.uri, raw.len())))?;
                        self.keys.insert(key.uri.clone(), k);
                        k
                    }
                };
                decrypt_aes128(&f.data, &k, &key.iv.unwrap_or_else(|| sequence_iv(seg.sequence)))?
            }
            None => f.data,
        };
        Ok((data, init, bytes, f.elapsed))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use url::Url;

    use super::*;
    use crate::hls::test_server::Server;

    fn root() -> std::path::PathBuf {
        format!("{}/tests/fixtures/hls", env!("CARGO_MANIFEST_DIR")).into()
    }

    fn start(server: &Server, playlist: &str, start: Start) -> Result<Track> {
        Track::start(TrackConfig {
            playlists: vec![Url::parse(&server.url(playlist)).unwrap()],
            variant: 0,
            start,
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Events until `End`/`Failed` or `limit`.
    fn collect(track: &Track, limit: Duration) -> Vec<TrackEvent> {
        let deadline = Instant::now() + limit;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if let Some(e) = track.recv(Duration::from_millis(50)) {
                let last = !matches!(e, TrackEvent::Segment(_));
                out.push(e);
                if last {
                    break;
                }
            }
        }
        out
    }

    fn segments(events: &[TrackEvent]) -> Vec<&SegmentData> {
        events.iter().filter_map(|e| if let TrackEvent::Segment(s) = e { Some(s) } else { None }).collect()
    }

    #[test]
    fn vod_track_delivers_all_segments_in_order_then_ends() {
        let server = Server::dir(root());
        let track = start(&server, "fmp4/index.m3u8", Start::At(Duration::ZERO)).unwrap();
        assert!(!track.is_live());
        assert_eq!(track.duration(), Some(Duration::from_millis(4120)));
        let events = collect(&track, Duration::from_secs(10));
        let segs = segments(&events);
        assert_eq!(segs.iter().map(|s| s.seq).collect::<Vec<_>>(), [0, 1]);
        assert_eq!((segs[0].start, segs[1].start), (Duration::ZERO, Duration::from_secs(2)));
        assert_eq!(segs[1].data, std::fs::read(root().join("fmp4/seg1.m4s")).unwrap());
        assert_eq!(**segs[0].init.as_ref().unwrap(), std::fs::read(root().join("fmp4/init.mp4")).unwrap());
        assert!(segs[0].bytes > 0);
        assert!(matches!(events.last(), Some(TrackEvent::End(_))));
        assert_eq!(server.hits("fmp4/init.mp4"), 1, "the init section is fetched once");
    }

    #[test]
    fn seek_restarts_at_the_segment_holding_the_target() {
        let server = Server::dir(root());
        let track = start(&server, "ts/index.m3u8", Start::At(Duration::ZERO)).unwrap();
        assert!(track.recv(Duration::from_secs(5)).is_some());
        track.seek(Duration::from_millis(2500));
        let events = collect(&track, Duration::from_secs(10));
        let after: Vec<_> = segments(&events).into_iter().filter(|s| s.epoch == 1).collect();
        assert_eq!(after.iter().map(|s| s.seq).collect::<Vec<_>>(), [2, 3, 4, 5]);
        assert_eq!(after[0].start, Duration::from_secs(2));
    }

    #[test]
    fn a_503_is_retried_and_a_404_fails() {
        let server = Server::dir(root());
        server.fail("ts/seg1.ts", 503, 1);
        let track = start(&server, "ts/index.m3u8", Start::At(Duration::ZERO)).unwrap();
        let events = collect(&track, Duration::from_secs(15));
        assert_eq!(segments(&events).len(), 6, "{events:?}");
        assert_eq!(server.hits("ts/seg1.ts"), 2);

        let server = Server::dir(root());
        server.fail("ts/seg2.ts", 404, usize::MAX);
        let track = start(&server, "ts/index.m3u8", Start::At(Duration::ZERO)).unwrap();
        let events = collect(&track, Duration::from_secs(15));
        assert_eq!(segments(&events).len(), 2);
        match events.last() {
            Some(TrackEvent::Failed(Error::Http(m), _)) => assert!(m.contains("404") && m.contains("seg2.ts"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(server.hits("ts/seg2.ts"), 2, "a 404 is retried once");
    }

    #[test]
    fn live_track_follows_the_window_and_ends_when_it_stops_moving() {
        let server = Server::dir(root());
        server.live("ts/live.m3u8", "ts/index.m3u8", root(), 3, Duration::from_secs(1), 3);
        let started = Instant::now();
        let track = Track::start(TrackConfig {
            playlists: vec![Url::parse(&server.url("ts/live.m3u8")).unwrap()],
            variant: 0,
            start: Start::LiveEdge,
            cancel: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        assert!(track.is_live());
        assert_eq!(track.duration(), None);
        let events = collect(&track, Duration::from_secs(20));
        let segs = segments(&events);
        assert_eq!(segs.iter().map(|s| s.seq).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5], "joined 3 target durations back, then followed: {events:?}");
        assert_eq!(segs[3].start, Duration::from_secs(3), "live time counts from the first segment played");
        assert!(matches!(events.last(), Some(TrackEvent::End(_))), "{:?}", events.last());
        let took = started.elapsed();
        assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(12), "{took:?}");
    }

    #[test]
    fn dropping_during_a_stalled_download_is_quick() {
        let server = Server::dir(root());
        server.stall("ts/seg0.ts", Duration::from_secs(30));
        let cancel = Arc::new(AtomicBool::new(false));
        let track = Track::start(TrackConfig {
            playlists: vec![Url::parse(&server.url("ts/index.m3u8")).unwrap()],
            variant: 0,
            start: Start::At(Duration::ZERO),
            cancel: cancel.clone(),
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        drop(track);
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn switching_variant_continues_at_the_next_segment() {
        let server = Server::dir(root());
        let ts = Url::parse(&server.url("ts/index.m3u8")).unwrap();
        // The same playlist twice: variant 1 is told apart by its URL.
        let other = Url::parse(&server.url("ts/index.m3u8?v=1")).unwrap();
        let track = Track::start(TrackConfig {
            playlists: vec![ts, other],
            variant: 0,
            start: Start::At(Duration::ZERO),
            cancel: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
        let first = match track.recv(Duration::from_secs(5)) {
            Some(TrackEvent::Segment(s)) => s,
            other => panic!("{other:?}"),
        };
        assert_eq!(first.variant, 0);
        track.switch(1);
        let events = collect(&track, Duration::from_secs(10));
        let segs = segments(&events);
        let seqs: Vec<u64> = segs.iter().map(|s| s.seq).collect();
        assert_eq!(seqs.windows(2).filter(|w| w[1] != w[0] + 1).count(), 0, "no gap or repeat: {seqs:?}");
        assert_eq!(segs.last().unwrap().variant, 1);
        assert_eq!(segs.last().unwrap().seq, 5);
    }

    #[test]
    fn a_failure_is_reported_even_when_a_command_is_waiting() {
        let server = Server::dir(root());
        server.fail("ts/seg3.ts", 404, usize::MAX);
        let track = start(&server, "ts/index.m3u8", Start::At(Duration::ZERO)).unwrap();
        // Nobody reads: segments 0-2 fill the channel, segment 3 fails and its Failed waits.
        std::thread::sleep(Duration::from_millis(2500));
        track.seek(Duration::from_secs(5)); // a command arrives while the Failed is waiting
        std::thread::sleep(Duration::from_millis(200));
        let events = collect(&track, Duration::from_secs(10));
        assert!(matches!(events.last(), Some(TrackEvent::Failed(..))), "{events:?}");
    }

    #[test]
    fn a_stopped_fetch_thread_is_reported_not_waited_for() {
        let server = Server::dir(root());
        let track = start(&server, "ts/index.m3u8", Start::At(Duration::ZERO)).unwrap();
        track.cancel.store(true, std::sync::atomic::Ordering::Relaxed); // the thread exits
        let events = collect(&track, Duration::from_secs(5));
        assert!(matches!(events.last(), Some(TrackEvent::Failed(..) | TrackEvent::End(_))), "{events:?}");
    }
}
