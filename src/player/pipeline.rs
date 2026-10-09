//! Demux and decode threads.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError};

use super::audio_thread::AudioLoop;
use super::speed::SpeedMonitor;
use super::PlayerState;
use crate::backend::Registry;
use super::{Command, PlayerEvent, Shared};
use crate::audio::OutputShared;
use crate::convert::yuv_to_bgra;
use crate::decode::{AudioDecoder, DecodedFrame, VideoDecoder, YuvFrame};
use crate::demux::{Demuxer, Packet, StreamInfo};
use crate::frame::VideoFrame;
use crate::{Error, Result};

const POLL: Duration = Duration::from_millis(50);
/// Catch-up: a frame this far behind the clock is not converted or queued...
const SKIP_LATE: Duration = Duration::from_millis(50);
/// ...unless it is this much media time after the last frame shown (the picture keeps moving
/// while catching up). Media time, not wall time, so it holds at any playback speed.
const MIN_SHOW_INTERVAL: Duration = Duration::from_millis(100);

/// Consecutive decode errors tolerated before the player gives up.
const MAX_DECODE_ERRORS: u32 = 3;

pub(super) enum Msg {
    Packet(Packet),
    /// A seek happened: drop decoder state; frames before `target` are decoded but not shown.
    Flush { generation: u64, target: Duration },
    Eof { generation: u64 },
}

/// The video stream's decoder, and where to find a faster one if it cannot keep up.
pub(crate) struct VideoPipe {
    pub stream: u32,
    pub decoder: Box<dyn VideoDecoder>,
    pub fallback: Option<Fallback>,
}

/// What the decode thread needs to open another backend's decoder for the same stream: used
/// when the decoder can't decode the stream at all (always), or can't keep up (`speed`).
pub(crate) struct Fallback {
    pub registry: Arc<Registry>,
    pub stream: StreamInfo,
    pub threads: usize,
    pub order: Option<Vec<&'static str>>,
    /// The backend currently decoding (never chosen as its own fallback).
    pub current: &'static str,
    /// Also switch when decoding is too slow (`PlayerConfig::auto_fallback`).
    pub speed: bool,
}

/// Everything the audio thread needs.
pub(crate) struct AudioPipe {
    pub info: StreamInfo,
    pub decoder: Box<dyn AudioDecoder>,
    pub producer: rtrb::Producer<f32>,
    pub out: Arc<OutputShared>,
}

/// Where the demux thread sends one stream's packets.
struct Route {
    stream: u32,
    tx: Option<Sender<Msg>>,
}

pub(super) fn spawn(
    shared: Arc<Shared>,
    demuxer: Box<dyn Demuxer>,
    video: Option<VideoPipe>,
    audio: Option<AudioPipe>,
    commands: Receiver<Command>,
    packet_queue_len: usize,
    pool: Arc<rayon::ThreadPool>,
) -> Result<Vec<JoinHandle<()>>> {
    let mut threads = Vec::new();
    let mut routes = Vec::new();
    let has_video = video.is_some();
    if let Some(VideoPipe { stream, decoder, fallback }) = video {
        let (tx, rx) = crossbeam_channel::bounded(packet_queue_len.max(1));
        routes.push(Route { stream, tx: Some(tx) });
        let s = shared.clone();
        threads.push(thread::Builder::new().name("video-decode".into()).spawn(move || {
            guarded(&s, |s| {
                let mut lp = DecodeLoop::new(decoder, pool);
                lp.fallback = fallback;
                lp.run(s, rx)
            })
        })?);
    }
    let mut seek_preroll = Duration::ZERO;
    if let Some(pipe) = audio {
        // With video, the audio channel must never block the demuxer: when video back-pressure
        // stalls demuxing, audio needs every packet up to that point or the audio clock (and so
        // video) would stall too. The bounded video channel caps how far ahead that can get.
        let (tx, rx) = if has_video {
            crossbeam_channel::unbounded()
        } else {
            crossbeam_channel::bounded(packet_queue_len.max(1))
        };
        routes.push(Route { stream: pipe.info.id, tx: Some(tx) });
        // Start early enough for the codec's pre-roll (Opus: 80 ms) even when a video keyframe
        // happens to sit exactly on the target; the extra audio/video is decoded and dropped.
        seek_preroll = pipe.info.seek_preroll;
        let s = shared.clone();
        threads.push(
            thread::Builder::new()
                .name("audio-decode".into())
                .spawn(move || guarded_audio(&s, |s| AudioLoop::new(pipe).run(s, rx)))?,
        );
    }
    let s = shared;
    threads.insert(
        0,
        thread::Builder::new()
            .name("video-demux".into())
            .spawn(move || guarded(&s, |s| demux_loop(s, demuxer, routes, seek_preroll, commands)))?,
    );
    Ok(threads)
}

/// Like `guarded`, but an audio panic only disables audio.
fn guarded_audio(shared: &Arc<Shared>, body: impl FnOnce(&Shared)) {
    if catch_unwind(AssertUnwindSafe(|| body(shared))).is_err() {
        shared.disable_audio("audio thread panicked");
    }
}

/// Converts a panic in a pipeline thread into a player error instead of tearing down the app.
fn guarded(shared: &Arc<Shared>, body: impl FnOnce(&Shared)) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| body(shared))) {
        let msg = panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        shared.fail(Error::Decode(format!("pipeline thread panicked: {msg}")));
    }
}

fn demux_loop(
    shared: &Shared,
    mut demuxer: Box<dyn Demuxer>,
    mut routes: Vec<Route>,
    seek_preroll: Duration,
    commands: Receiver<Command>,
) {
    let mut generation = 0;
    let mut eof = false;
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let command = if eof {
            match commands.recv_timeout(POLL) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match commands.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        };
        if let Some(Command::Seek { target, generation: g }) = command {
            generation = g;
            eof = false;
            // Audio-only: start early enough for the codec's pre-roll; samples before the target
            // are decoded and dropped.
            if let Err(e) = demuxer.seek(target.saturating_sub(seek_preroll)) {
                shared.fail(e);
                return;
            }
            if !broadcast(shared, &mut routes, &commands, |_| Msg::Flush { generation, target }) {
                return;
            }
            continue;
        }
        if eof {
            continue;
        }
        crate::player::Diag::set(&shared.diag.demux, 1, Duration::ZERO);
        match demuxer.next_packet() {
            Ok(Some(mut p)) => {
                p.generation = generation;
                let audio = shared.audio_out.is_some() && routes.last().is_some_and(|r| r.stream == p.stream) && routes.len() > 1;
                crate::player::Diag::set(&shared.diag.demux, if audio { 2 } else { 3 }, p.pts);
                for (r, slot) in routes.iter().zip([&shared.diag.video_queued, &shared.diag.audio_queued]) {
                    slot.store(r.tx.as_ref().map_or(0, |t| t.len() as u64), Ordering::Relaxed);
                }
                if let Some(route) = routes.iter_mut().find(|r| r.stream == p.stream)
                    && !send_to(shared, route, &commands, Msg::Packet(p))
                {
                    return;
                }
            }
            Ok(None) => {
                crate::player::Diag::set(&shared.diag.demux, 4, Duration::ZERO);
                eof = true;
                if !broadcast(shared, &mut routes, &commands, |_| Msg::Eof { generation }) {
                    return;
                }
            }
            Err(e) => {
                shared.fail(e);
                return;
            }
        }
    }
}

fn broadcast(shared: &Shared, routes: &mut [Route], commands: &Receiver<Command>, msg: impl Fn(u32) -> Msg) -> bool {
    routes.iter_mut().all(|r| {
        let m = msg(r.stream);
        send_to(shared, r, commands, m)
    })
}

/// Sends to one route. A closed route (its decoder thread gave up) is dropped silently.
/// Returns `false` when the pipeline should stop.
fn send_to(shared: &Shared, route: &mut Route, commands: &Receiver<Command>, msg: Msg) -> bool {
    let Some(tx) = &route.tx else { return true };
    match send(shared, tx, commands, msg) {
        SendOutcome::Sent | SendOutcome::Superseded => true,
        SendOutcome::Closed => {
            route.tx = None;
            !shared.shutdown.load(Ordering::SeqCst)
        }
        SendOutcome::Shutdown => false,
    }
}

enum SendOutcome {
    Sent,
    /// A newer command is waiting, which makes this message stale.
    Superseded,
    Closed,
    Shutdown,
}

/// Sends with back-pressure, giving up on the message if a new command arrives meanwhile.
fn send(shared: &Shared, tx: &Sender<Msg>, commands: &Receiver<Command>, mut msg: Msg) -> SendOutcome {
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return SendOutcome::Shutdown;
        }
        match tx.send_timeout(msg, POLL) {
            Ok(()) => return SendOutcome::Sent,
            Err(SendTimeoutError::Timeout(m)) => {
                if !commands.is_empty() {
                    return SendOutcome::Superseded;
                }
                msg = m;
            }
            Err(SendTimeoutError::Disconnected(_)) => return SendOutcome::Closed,
        }
    }
}

struct DecodeLoop {
    decoder: Box<dyn VideoDecoder>,
    pool: Arc<rayon::ThreadPool>,
    generation: u64,
    /// Accurate-seek target: frames up to here are held back, only the last one is shown.
    target: Option<Duration>,
    held: Option<YuvFrame>,
    errors: u32,
    waiting_for_keyframe: bool,
    /// Set until the one allowed switch to another backend has happened.
    fallback: Option<Fallback>,
    /// The current decoder has produced a frame (an error before that means it can't decode
    /// this stream at all).
    decoded_any: bool,
    monitor: SpeedMonitor,
    /// Time spent inside the decoder since the last decoded frame.
    busy: Duration,
    /// Pts of the last frame converted and queued.
    last_shown: Option<Duration>,
    scaler: crate::scale::FrameScaler,
}

impl DecodeLoop {
    fn new(decoder: Box<dyn VideoDecoder>, pool: Arc<rayon::ThreadPool>) -> Self {
        Self {
            decoder,
            pool,
            generation: 0,
            target: None,
            held: None,
            errors: 0,
            waiting_for_keyframe: false,
            fallback: None,
            decoded_any: false,
            monitor: SpeedMonitor::new(),
            busy: Duration::ZERO,
            last_shown: None,
            scaler: crate::scale::FrameScaler::default(),
        }
    }

    fn run(mut self, shared: &Shared, rx: Receiver<Msg>) {
        loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
            crate::player::Diag::set(&shared.diag.video, 1, self.last_shown.unwrap_or_default());
            let msg = match rx.recv_timeout(POLL) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            let keep_going = match msg {
                Msg::Flush { generation, target } => {
                    self.decoder.flush();
                    self.generation = generation;
                    self.target = Some(target);
                    self.held = None;
                    self.errors = 0;
                    self.waiting_for_keyframe = false;
                    self.monitor.reset();
                    self.busy = Duration::ZERO;
                    self.last_shown = None;
                    true
                }
                Msg::Packet(p) => self.on_packet(shared, p),
                Msg::Eof { generation } if generation == self.generation => self.on_eof(shared),
                Msg::Eof { .. } => true,
            };
            if !keep_going {
                return;
            }
        }
    }

    fn is_stale(&self, shared: &Shared, generation: u64) -> bool {
        generation != self.generation || generation < shared.generation.load(Ordering::SeqCst)
    }

    fn on_packet(&mut self, shared: &Shared, p: Packet) -> bool {
        crate::player::Diag::set(&shared.diag.video, 2, p.pts);
        if self.is_stale(shared, p.generation) || (self.waiting_for_keyframe && !p.keyframe) {
            return true;
        }
        self.waiting_for_keyframe = false;
        self.decoder.set_output_hint(shared.max_output_size());
        let start = Instant::now();
        let sent = self.decoder.send_packet(&p);
        self.busy += start.elapsed();
        if let Err(e) = sent {
            return self.on_decode_error(shared, e);
        }
        self.drain(shared)
    }

    fn on_decode_error(&mut self, shared: &Shared, e: Error) -> bool {
        self.errors += 1;
        log::warn!("video decode error ({}/{MAX_DECODE_ERRORS}): {e}", self.errors);
        // A decoder that fails before its first frame (e.g. a GPU given a profile it lacks) or
        // keeps failing hands the stream to the next backend that supports it.
        if (!self.decoded_any || self.errors >= MAX_DECODE_ERRORS)
            && self.switch_backend(shared, &format!("can't decode this video ({e})"))
        {
            self.errors = 0;
            return true;
        }
        if self.errors >= MAX_DECODE_ERRORS {
            shared.fail(e);
            return false;
        }
        self.decoder.flush();
        self.waiting_for_keyframe = true;
        true
    }

    fn drain(&mut self, shared: &Shared) -> bool {
        loop {
            let start = Instant::now();
            let received = self.decoder.receive_frame();
            self.busy += start.elapsed();
            match received {
                Ok(Some(DecodedFrame::Yuv(f))) => {
                    self.errors = 0;
                    self.decoded_any = true;
                    if !self.on_frame(shared, f) {
                        return false;
                    }
                }
                Ok(None) => return true,
                Err(e) => return self.on_decode_error(shared, e),
            }
        }
    }

    fn on_frame(&mut self, shared: &Shared, f: YuvFrame) -> bool {
        if let Some(target) = self.target {
            if f.pts <= target {
                self.held = Some(f);
                return true;
            }
            self.target = None;
            if let Some(held) = self.held.take()
                && !self.present(shared, held)
            {
                return false;
            }
        }
        self.present(shared, f)
    }

    /// Feeds the speed monitor (only while actually playing) and switches backend, once, when
    /// decoding has been too slow for too long.
    fn check_speed(&mut self, shared: &Shared, cost: Duration, pts: Duration) {
        if !self.fallback.as_ref().is_some_and(|f| f.speed) {
            return;
        }
        if shared.state() != PlayerState::Playing {
            self.monitor.reset();
            return;
        }
        let now = Instant::now();
        self.monitor.record_frame(now, cost, pts);
        self.monitor.record_lateness(now, shared.clock.now().saturating_sub(pts));
        // At most one frame waiting: the renderer is eating frames as fast as we make them.
        let behind = shared.queue.len() <= 1;
        self.monitor.record_drops(now, shared.queue.dropped(), behind);
        if self.monitor.too_slow(now) {
            self.switch_backend(shared, "decoding is too slow for this video");
        }
    }

    /// Moves the stream to the next backend that supports it (once). Returns whether it did.
    fn switch_backend(&mut self, shared: &Shared, why: &str) -> bool {
        let Some(f) = self.fallback.take() else { return false };
        let codec = &f.stream.codec;
        match f.registry.open_video_decoder_except(&f.stream, f.threads, f.order.as_deref(), Some(f.current)) {
            Ok((name, decoder)) => {
                let _ = shared.events.send(PlayerEvent::Warning(format!(
                    "{} {codec} {why}; switching to {name}",
                    f.current
                )));
                self.decoder = decoder;
                self.decoded_any = false;
                *shared.video_backend.lock().unwrap() = Some(name);
                if shared.seekable {
                    // Restart decoding from where playback is: the demuxer goes back to the
                    // keyframe before it and frames up to here are decoded but not shown. A user
                    // seek since then already restarts decoding, so it is left alone.
                    shared.seek_to_now_if_current(self.generation);
                } else {
                    self.waiting_for_keyframe = true;
                }
                true
            }
            Err(e) => {
                log::info!("{} {codec} {why}, and no other backend can take over: {e}", f.current);
                let _ = shared.events.send(PlayerEvent::Warning(format!(
                    "{} {codec} {why}, and no other decoder is available",
                    f.current
                )));
                false
            }
        }
    }

    fn on_eof(&mut self, shared: &Shared) -> bool {
        crate::player::Diag::set(&shared.diag.video, 4, self.last_shown.unwrap_or_default());
        self.decoder.send_eof();
        if !self.drain(shared) {
            return false;
        }
        self.target = None;
        if let Some(held) = self.held.take()
            && !self.present(shared, held)
        {
            return false;
        }
        // Wait for the renderer to consume the remaining frames, unless a seek or shutdown happens.
        loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            if shared.generation.load(Ordering::SeqCst) != self.generation {
                return true;
            }
            if shared.queue.is_empty() {
                shared.stream_finished(self.generation, true);
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Converts and queues a frame. Returns `false` only on shutdown.
    fn present(&mut self, shared: &Shared, f: YuvFrame) -> bool {
        // Catch-up: while behind the clock, don't spend time converting frames that are already
        // late, so the decoder can get ahead again; still show one every MIN_SHOW_INTERVAL.
        let first = shared.ready_generation.load(Ordering::SeqCst) != self.generation;
        if !first
            && shared.clock.now().saturating_sub(f.pts) > SKIP_LATE
            && self.last_shown.is_some_and(|p| f.pts.saturating_sub(p) < MIN_SHOW_INTERVAL)
            && shared.state() == PlayerState::Playing
        {
            let cost = std::mem::take(&mut self.busy);
            self.check_speed(shared, cost, f.pts);
            shared.queue.note_skipped();
            return true;
        }
        let mut bgra = Vec::new();
        let start = Instant::now();
        let scaler = &mut self.scaler;
        let max = shared.max_output_size();
        let converted = self.pool.install(|| {
            let scaled = max.and_then(|max| scaler.downscale(&f, max));
            let f = scaled.as_ref().unwrap_or(&f);
            yuv_to_bgra(f, &mut bgra).map(|()| (f.width, f.height))
        });
        let (out_w, out_h) = match converted {
            Ok(size) => size,
            Err(e) => return self.on_decode_error(shared, e),
        };
        let cost = std::mem::take(&mut self.busy) + start.elapsed();
        self.check_speed(shared, cost, f.pts);
        let frame = VideoFrame::Cpu { width: out_w, height: out_h, bgra: bgra.into(), pts: f.pts };
        let first = shared.ready_generation.load(Ordering::SeqCst) != self.generation;
        {
            let _guard = shared.seek_lock.lock().unwrap();
            if self.generation != shared.generation.load(Ordering::SeqCst) {
                // Superseded by a newer seek: must not touch the clock or the queue.
                return !shared.shutdown.load(Ordering::SeqCst);
            }
            if first && !shared.audio_master && f.pts > shared.clock.now() {
                // Seeked before the first frame: start the clock at the first frame instead.
                shared.clock.set(f.pts);
            }
        }
        let pts = frame.pts();
        crate::player::Diag::set(&shared.diag.video, 3, pts);
        if !shared.queue.push(self.generation, frame) {
            return !shared.shutdown.load(Ordering::SeqCst);
        }
        self.last_shown = Some(pts);
        if first && shared.frame_ready(self.generation) {
            let _ = shared.events.send(PlayerEvent::FrameReady);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::player::PlayerState;
    use crate::clock::{Clock, MockClock};
    use crate::decode::{ColorMatrix, PixelLayout, VideoDecoder};
    use crate::frame::FrameQueue;

    struct NoDecoder;
    impl VideoDecoder for NoDecoder {
        fn send_packet(&mut self, _: &Packet) -> Result<()> {
            Ok(())
        }
        fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) {}
    }

    type Hints = Arc<Mutex<Vec<Option<(u32, u32)>>>>;

    /// Records the output hints it is given.
    struct HintRecorder(Hints);
    impl VideoDecoder for HintRecorder {
        fn send_packet(&mut self, _: &Packet) -> Result<()> {
            Ok(())
        }
        fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) {}
        fn set_output_hint(&mut self, max: Option<(u32, u32)>) {
            self.0.lock().unwrap().push(max);
        }
    }

    /// Fails on every packet, like a GPU decoder given a stream variant it can't decode.
    struct Broken;
    impl VideoDecoder for Broken {
        fn send_packet(&mut self, _: &Packet) -> Result<()> {
            Err(Error::Decode("cannot start".into()))
        }
        fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
            Ok(None)
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn a_decoder_that_cannot_start_hands_over_to_the_next_backend() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut shared = shared(Arc::new(MockClock::new()));
        shared.events = tx;
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let mut registry = Registry::empty();
        registry.register(Arc::new(Spare));
        let mut decode = DecodeLoop::new(Box::new(Broken), pool);
        decode.fallback = Some(Fallback {
            registry: Arc::new(registry),
            stream: StreamInfo::new(1, crate::demux::StreamKind::Video, crate::demux::Codec::H264),
            threads: 1,
            order: None,
            current: "broken",
            speed: false, // not about speed: this decoder can't play the stream at all
        });
        let packet = Packet { stream: 1, pts: Duration::ZERO, keyframe: true, data: vec![0], generation: 0 };
        assert!(decode.on_packet(&shared, packet));
        assert!(decode.fallback.is_none(), "switched");
        assert_eq!(*shared.video_backend.lock().unwrap(), Some("spare"));
        assert!(!shared.state().is_error());
        assert!(rx.try_iter().any(|e| matches!(e, PlayerEvent::Warning(w) if w.contains("spare"))));
    }

    #[test]
    fn decoders_get_the_display_size_before_each_packet() {
        let shared = shared(Arc::new(MockClock::new()));
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let hints = Arc::new(Mutex::new(Vec::new()));
        let mut decode = DecodeLoop::new(Box::new(HintRecorder(hints.clone())), pool);
        let packet = || Packet { stream: 1, pts: Duration::ZERO, keyframe: true, data: vec![0], generation: 0 };
        shared.max_output_size.store(crate::player::pack_size(Some((1280, 720))), Ordering::Relaxed);
        assert!(decode.on_packet(&shared, packet()));
        shared.max_output_size.store(crate::player::pack_size(None), Ordering::Relaxed);
        assert!(decode.on_packet(&shared, packet()));
        assert_eq!(*hints.lock().unwrap(), vec![Some((1280, 720)), None]);
    }

    fn shared(clock: Arc<MockClock>) -> Shared {
        Shared {
            state: Mutex::new(PlayerState::Paused),
            clock,
            queue: FrameQueue::new(4),
            generation: AtomicU64::new(0),
            ready_generation: AtomicU64::new(u64::MAX),
            wants_play: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            last_frame: Mutex::new(None),
            events: crossbeam_channel::unbounded().0,
            duration: None,
            seek_lock: Mutex::new(()),
            has_video: true,
            audio_active: AtomicBool::new(false),
            audio_master: false,
            audio_out: None,
            volume: Arc::new(crate::audio::Volume::default()),
            video_done: AtomicU64::new(u64::MAX),
            audio_done: AtomicU64::new(u64::MAX),
            ended_generation: AtomicU64::new(u64::MAX),
            commands: crossbeam_channel::unbounded().0,
            seekable: true,
            video_backend: Mutex::new(None),
            max_output_size: AtomicU64::new(0),
            diag: Default::default(),
        }
    }

    fn frame(pts: Duration) -> YuvFrame {
        YuvFrame {
            width: 2,
            height: 2,
            layout: PixelLayout::I420,
            planes: [vec![16; 4], vec![128], vec![128]],
            strides: [2, 1, 1],
            matrix: ColorMatrix::Bt601,
            full_range: false,
            pts,
        }
    }

    /// A backend that opens `NoDecoder` for anything: the fallback target in tests.
    struct Spare;
    impl crate::backend::Backend for Spare {
        fn name(&self) -> &'static str {
            "spare"
        }
        fn priority(&self) -> i32 {
            0
        }
        fn supports_container(&self, _: crate::demux::ContainerFormat) -> bool {
            false
        }
        fn open_demuxer(
            &self,
            _: crate::demux::ContainerFormat,
            _: Box<dyn crate::source::MediaSource>,
        ) -> Result<Box<dyn Demuxer>> {
            unreachable!()
        }
        fn supports_video(&self, _: &StreamInfo) -> bool {
            true
        }
        fn open_video_decoder(&self, _: &StreamInfo, _: usize) -> Result<Box<dyn VideoDecoder>> {
            Ok(Box::new(NoDecoder))
        }
    }

    #[test]
    fn frames_staying_behind_the_clock_switch_backend_even_when_cheap() {
        // Decode cost is negligible and nothing is dropped (one frame consumed per frame made),
        // but every frame reaches the queue 300 ms behind the playback clock.
        let clock = Arc::new(MockClock::new());
        let shared = shared(clock.clone());
        *shared.state.lock().unwrap() = PlayerState::Playing;
        shared.ready_generation.store(0, Ordering::SeqCst);
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let mut registry = Registry::empty();
        registry.register(Arc::new(Spare));
        let mut decode = DecodeLoop::new(Box::new(NoDecoder), pool);
        decode.fallback = Some(Fallback {
            registry: Arc::new(registry),
            stream: StreamInfo::new(1, crate::demux::StreamKind::Video, crate::demux::Codec::Av1),
            threads: 1,
            order: None,
            current: "slow",
            speed: true,
        });
        let start = Instant::now();
        let mut pts = Duration::ZERO;
        while decode.fallback.is_some() {
            assert!(start.elapsed() < Duration::from_secs(4), "never switched");
            clock.set(pts + Duration::from_millis(300));
            decode.present(&shared, frame(pts));
            shared.queue.frame_for(clock.now());
            pts += Duration::from_millis(16);
            std::thread::sleep(Duration::from_millis(16));
        }
        assert_eq!(*shared.video_backend.lock().unwrap(), Some("spare"));
        assert_eq!(shared.queue.dropped(), 0, "the drop rule did not do it");
    }

    #[test]
    fn late_frames_are_skipped_but_the_picture_still_updates() {
        let clock = Arc::new(MockClock::new());
        let shared = shared(clock.clone());
        *shared.state.lock().unwrap() = PlayerState::Playing;
        shared.ready_generation.store(0, Ordering::SeqCst);
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let mut decode = DecodeLoop::new(Box::new(NoDecoder), pool);
        // The decoder is 1 s behind and catching up: 30 frames arrive within ~50 ms.
        clock.set(Duration::from_secs(2));
        let mut shown = 0;
        for i in 0..30u64 {
            decode.present(&shared, frame(Duration::from_millis(1000 + i * 16)));
            if shared.queue.frame_for(clock.now()).is_some() {
                shown += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // Shown: 1000, 1112, 1224, 1336, 1448 ms — one per 100 ms of video.
        assert_eq!(shown, 5, "late frames are not converted, except one per 100 ms of video");
        assert_eq!(shared.queue.skipped(), 25);
        // On time again: every frame is shown.
        for i in 0..5u64 {
            let pts = Duration::from_millis(2100 + i * 16);
            clock.set(pts);
            decode.present(&shared, frame(pts));
            assert!(shared.queue.frame_for(pts).is_some(), "on-time frame {i} skipped");
        }
    }

    #[test]
    fn fallback_resync_never_overrides_a_newer_user_seek() {
        let clock = Arc::new(MockClock::new());
        let shared = shared(clock.clone());
        // The decode thread decided to resync while in generation 0...
        clock.set(Duration::from_secs(5));
        // ...but the user sought to 60 s first.
        shared.seek(Duration::from_secs(60));
        assert!(!shared.seek_to_now_if_current(0), "stale resync must not seek");
        assert_eq!(clock.now(), Duration::from_secs(60));
        assert_eq!(shared.generation.load(Ordering::SeqCst), 1);
        // In the current generation it does seek, to where playback is.
        assert!(shared.seek_to_now_if_current(1));
        assert_eq!(shared.generation.load(Ordering::SeqCst), 2);
        assert_eq!(clock.now(), Duration::from_secs(60));
    }

    #[test]
    fn frame_from_superseded_seek_does_not_move_the_clock() {
        let clock = Arc::new(MockClock::new());
        let shared = shared(clock.clone());
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let mut decode = DecodeLoop::new(Box::new(NoDecoder), pool);
        // Decode thread is still working on seek #1 (to ~10 s)...
        decode.generation = 1;
        // ...when seek #2 (to 2 s) happens on the UI thread.
        shared.generation.store(2, Ordering::SeqCst);
        shared.queue.clear(2);
        clock.set(Duration::from_secs(2));

        decode.present(&shared, frame(Duration::from_millis(9_990)));

        assert_eq!(clock.now(), Duration::from_secs(2), "stale frame moved the clock");
        assert!(shared.queue.is_empty());
        assert_ne!(shared.ready_generation.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn play_pause_and_first_frame_never_leave_state_and_clock_disagreeing() {
        // Race the UI's play/pause against the decode thread's "first frame ready".
        for i in 0..3000 {
            let clock = Arc::new(MockClock::new());
            let s = Arc::new(shared(clock.clone()));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (s2, b2) = (s.clone(), barrier.clone());
            let decoder = std::thread::spawn(move || {
                b2.wait();
                s2.frame_ready(0);
            });
            barrier.wait();
            s.request_play();
            if i % 2 == 1 {
                s.request_pause();
            }
            decoder.join().unwrap();
            let state = s.state();
            match state {
                PlayerState::Paused => assert!(clock.is_paused(), "iteration {i}: Paused but the clock runs"),
                PlayerState::Playing => assert!(!clock.is_paused(), "iteration {i}: Playing but the clock is stopped"),
                PlayerState::Buffering => panic!("iteration {i}: stuck in Buffering although the frame is ready"),
                other => panic!("iteration {i}: unexpected {other:?}"),
            }
        }
    }
}
