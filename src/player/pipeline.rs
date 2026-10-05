//! Demux and decode threads.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError};

use super::audio_thread::AudioLoop;
use super::{Command, PlayerEvent, Shared};
use crate::audio::OutputShared;
use crate::convert::yuv_to_bgra;
use crate::decode::{AudioDecoder, DecodedFrame, VideoDecoder, YuvFrame};
use crate::demux::{Demuxer, Packet, StreamInfo};
use crate::frame::VideoFrame;
use crate::{Error, Result};

const POLL: Duration = Duration::from_millis(50);
/// Consecutive decode errors tolerated before the player gives up.
const MAX_DECODE_ERRORS: u32 = 3;

pub(super) enum Msg {
    Packet(Packet),
    /// A seek happened: drop decoder state; frames before `target` are decoded but not shown.
    Flush { generation: u64, target: Duration },
    Eof { generation: u64 },
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
    video: Option<(u32, Box<dyn VideoDecoder>)>,
    audio: Option<AudioPipe>,
    commands: Receiver<Command>,
    packet_queue_len: usize,
    pool: Arc<rayon::ThreadPool>,
) -> Result<Vec<JoinHandle<()>>> {
    let mut threads = Vec::new();
    let mut routes = Vec::new();
    let has_video = video.is_some();
    if let Some((stream, decoder)) = video {
        let (tx, rx) = crossbeam_channel::bounded(packet_queue_len.max(1));
        routes.push(Route { stream, tx: Some(tx) });
        let s = shared.clone();
        threads.push(
            thread::Builder::new()
                .name("video-decode".into())
                .spawn(move || guarded(&s, |s| DecodeLoop::new(decoder, pool).run(s, rx)))?,
        );
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
        match demuxer.next_packet() {
            Ok(Some(mut p)) => {
                p.generation = generation;
                if let Some(route) = routes.iter_mut().find(|r| r.stream == p.stream)
                    && !send_to(shared, route, &commands, Msg::Packet(p))
                {
                    return;
                }
            }
            Ok(None) => {
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
        }
    }

    fn run(mut self, shared: &Shared, rx: Receiver<Msg>) {
        loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
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
        if self.is_stale(shared, p.generation) || (self.waiting_for_keyframe && !p.keyframe) {
            return true;
        }
        self.waiting_for_keyframe = false;
        if let Err(e) = self.decoder.send_packet(&p) {
            return self.on_decode_error(shared, e);
        }
        self.drain(shared)
    }

    fn on_decode_error(&mut self, shared: &Shared, e: Error) -> bool {
        self.errors += 1;
        log::warn!("video decode error ({}/{MAX_DECODE_ERRORS}): {e}", self.errors);
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
            match self.decoder.receive_frame() {
                Ok(Some(DecodedFrame::Yuv(f))) => {
                    self.errors = 0;
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

    fn on_eof(&mut self, shared: &Shared) -> bool {
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
        let mut bgra = Vec::new();
        if let Err(e) = self.pool.install(|| yuv_to_bgra(&f, &mut bgra)) {
            return self.on_decode_error(shared, e);
        }
        let frame = VideoFrame::Cpu { width: f.width, height: f.height, bgra: bgra.into(), pts: f.pts };
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
        if !shared.queue.push(self.generation, frame) {
            return !shared.shutdown.load(Ordering::SeqCst);
        }
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
