//! Demux and decode threads.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError};

use super::{Command, PlayerEvent, PlayerState, Shared};
use crate::convert::yuv_to_bgra;
use crate::decode::{DecodedFrame, VideoDecoder, YuvFrame};
use crate::demux::{Demuxer, Packet};
use crate::frame::VideoFrame;
use crate::{Error, Result};

const POLL: Duration = Duration::from_millis(50);
/// Consecutive decode errors tolerated before the player gives up.
const MAX_DECODE_ERRORS: u32 = 3;

enum Msg {
    Packet(Packet),
    /// A seek happened: drop decoder state; frames before `target` are decoded but not shown.
    Flush { generation: u64, target: Duration },
    Eof { generation: u64 },
}

pub(super) fn spawn(
    shared: Arc<Shared>,
    demuxer: Box<dyn Demuxer>,
    decoder: Box<dyn VideoDecoder>,
    video_stream: u32,
    commands: Receiver<Command>,
    packet_queue_len: usize,
    pool: Arc<rayon::ThreadPool>,
) -> Result<Vec<JoinHandle<()>>> {
    let (tx, rx) = crossbeam_channel::bounded(packet_queue_len.max(1));
    let s = shared.clone();
    let demux = thread::Builder::new()
        .name("video-demux".into())
        .spawn(move || guarded(&s, |s| demux_loop(s, demuxer, video_stream, commands, tx)))?;
    let s = shared;
    let decode = thread::Builder::new()
        .name("video-decode".into())
        .spawn(move || guarded(&s, |s| DecodeLoop::new(decoder, pool).run(s, rx)))?;
    Ok(vec![demux, decode])
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
    video_stream: u32,
    commands: Receiver<Command>,
    tx: Sender<Msg>,
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
            if let Err(e) = demuxer.seek(target) {
                shared.fail(e);
                return;
            }
            if !send(shared, &tx, &commands, Msg::Flush { generation, target }) {
                return;
            }
            continue;
        }
        if eof {
            continue;
        }
        match demuxer.next_packet() {
            Ok(Some(mut p)) if p.stream == video_stream => {
                p.generation = generation;
                if !send(shared, &tx, &commands, Msg::Packet(p)) {
                    return;
                }
            }
            Ok(Some(_)) => {} // non-video streams are ignored in phase 1
            Ok(None) => {
                eof = true;
                if !send(shared, &tx, &commands, Msg::Eof { generation }) {
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

/// Sends with back-pressure. Gives up on the message (returning `true`) if a new command is
/// waiting, because that command makes it stale. Returns `false` when the pipeline should stop.
fn send(shared: &Shared, tx: &Sender<Msg>, commands: &Receiver<Command>, mut msg: Msg) -> bool {
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        match tx.send_timeout(msg, POLL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(m)) => {
                if !commands.is_empty() {
                    return true;
                }
                msg = m;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
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
                shared.clock.pause();
                shared.wants_play.store(false, Ordering::SeqCst);
                shared.set_state(PlayerState::Ended);
                let _ = shared.events.send(PlayerEvent::Ended);
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
        if first && f.pts > shared.clock.now() {
            // Seeked before the first frame: start the clock at the first frame instead.
            shared.clock.set(f.pts);
        }
        if !shared.queue.push(self.generation, frame) {
            return !shared.shutdown.load(Ordering::SeqCst);
        }
        if first {
            shared.ready_generation.store(self.generation, Ordering::SeqCst);
            let _ = shared.events.send(PlayerEvent::FrameReady);
            if shared.wants_play.load(Ordering::SeqCst) {
                shared.clock.resume();
                if shared.state() == PlayerState::Buffering {
                    shared.set_state(PlayerState::Playing);
                }
            }
        }
        true
    }
}
