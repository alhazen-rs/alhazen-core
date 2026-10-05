//! The audio decode thread: decode, align to the seek target, remix, resample, feed the device.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use super::pipeline::{AudioPipe, Msg};
use super::{PlayerState, Shared};
use crate::audio::{OutputShared, Resampler, push_frames, remix};
use crate::decode::{AudioBuffer, AudioDecoder};

const POLL: Duration = Duration::from_millis(50);
const MAX_DECODE_ERRORS: u32 = 3;

pub(super) struct AudioLoop {
    decoder: Box<dyn AudioDecoder>,
    producer: rtrb::Producer<f32>,
    out: Arc<OutputShared>,
    resampler: Option<Resampler>,
    generation: u64,
    /// Accurate-seek target: samples before it are dropped; a gap after it is filled with silence.
    target: Option<Duration>,
    /// Frames pushed into the ring so far (the ring's sequence numbers).
    pushed: u64,
    errors: u32,
}

impl AudioLoop {
    pub fn new(pipe: AudioPipe) -> Self {
        Self {
            decoder: pipe.decoder,
            producer: pipe.producer,
            out: pipe.out,
            resampler: None,
            generation: 0,
            target: Some(Duration::ZERO),
            pushed: 0,
            errors: 0,
        }
    }

    pub fn run(mut self, shared: &Shared, rx: Receiver<Msg>) {
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
                    if let Some(r) = &mut self.resampler {
                        r.reset();
                    }
                    self.generation = generation;
                    self.target = Some(target);
                    self.errors = 0;
                    // Everything pushed so far predates the seek.
                    self.out.discard_until.store(self.pushed, Ordering::SeqCst);
                    true
                }
                Msg::Packet(p) => {
                    if p.generation != self.generation || p.generation < shared.generation.load(Ordering::SeqCst) {
                        true
                    } else {
                        match self.decoder.send_packet(&p) {
                            Ok(()) => self.drain(shared),
                            Err(e) => self.on_error(shared, &e.to_string()),
                        }
                    }
                }
                Msg::Eof { generation } if generation == self.generation => self.on_eof(shared),
                Msg::Eof { .. } => true,
            };
            if !keep_going {
                return;
            }
        }
    }

    fn on_error(&mut self, shared: &Shared, why: &str) -> bool {
        self.errors += 1;
        log::warn!("audio decode error ({}/{MAX_DECODE_ERRORS}): {why}", self.errors);
        if self.errors >= MAX_DECODE_ERRORS {
            shared.disable_audio(&format!("decode errors: {why}"));
            return false;
        }
        true
    }

    fn drain(&mut self, shared: &Shared) -> bool {
        loop {
            match self.decoder.receive_samples() {
                Ok(Some(buf)) => {
                    self.errors = 0;
                    if !self.play(shared, buf) {
                        return false;
                    }
                }
                Ok(None) => return true,
                Err(e) => return self.on_error(shared, &e.to_string()),
            }
        }
    }

    /// Returns `false` only on shutdown.
    fn play(&mut self, shared: &Shared, buf: AudioBuffer) -> bool {
        let ch = self.out.channels;
        let rate = buf.rate.max(1);
        let mut samples = remix(&buf.samples, buf.channels, ch);
        if let Some(target) = self.target {
            if buf.pts + buf.duration() <= target {
                return true; // entirely before the seek target
            }
            let frames_between = |a: Duration, b: Duration| ((b - a).as_nanos() * rate as u128 / 1_000_000_000) as usize;
            if buf.pts < target {
                let drop = frames_between(buf.pts, target).min(samples.len() / ch as usize);
                samples.drain(..drop * ch as usize);
            } else if buf.pts > target {
                let gap = frames_between(target, buf.pts);
                samples.splice(0..0, std::iter::repeat_n(0.0, gap * ch as usize));
            }
            self.target = None;
        }
        if self.resampler.as_ref().is_none_or(|r| r.rates() != (rate, self.out.rate)) {
            self.resampler = Some(Resampler::new(rate, self.out.rate, ch));
        }
        let samples = self.resampler.as_mut().unwrap().process(&samples);
        let generation = self.generation;
        let written = push_frames(&mut self.producer, ch, &samples, || {
            !shared.shutdown.load(Ordering::SeqCst) && shared.generation.load(Ordering::SeqCst) == generation
        });
        self.pushed += written as u64;
        if shared.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        if written > 0 && !shared.has_video {
            self.mark_ready(shared);
        }
        true
    }

    /// Audio-only media: the first audio of a generation is what playback waits for.
    fn mark_ready(&self, shared: &Shared) {
        if shared.ready_generation.swap(self.generation, Ordering::SeqCst) == self.generation {
            return;
        }
        if shared.wants_play.load(Ordering::SeqCst) {
            shared.clock.resume();
            if shared.state() == PlayerState::Buffering {
                shared.set_state(PlayerState::Playing);
            }
        }
    }

    /// Waits until the device has played everything queued, then reports the stream finished.
    fn on_eof(&mut self, shared: &Shared) -> bool {
        if !self.drain(shared) {
            return false;
        }
        let capacity = self.producer.buffer().capacity();
        loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            if shared.generation.load(Ordering::SeqCst) != self.generation {
                return true;
            }
            if self.producer.slots() == capacity || self.out.failed.load(Ordering::Relaxed) {
                shared.stream_finished(self.generation, false);
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
