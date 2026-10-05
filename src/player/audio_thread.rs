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
/// Timestamp differences below this are container rounding, not gaps or overlaps.
const TOLERANCE: Duration = Duration::from_millis(4);

/// How a decoded buffer lines up with where the audio timeline expects the next sample.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Align {
    /// Starts where expected (within `TOLERANCE`).
    Keep,
    /// Ends before the expected time (seek pre-roll, or an overlap): drop it.
    Skip,
    /// Starts this many frames too early: drop them.
    Drop(usize),
    /// Starts this many frames too late (a gap, or a skipped packet): play silence first.
    Silence(usize),
}

fn frames_between(a: Duration, b: Duration, rate: u32) -> usize {
    ((b - a).as_nanos() * rate as u128 / 1_000_000_000) as usize
}

pub(super) fn align(expected: Duration, pts: Duration, frames: usize, rate: u32) -> Align {
    let end = pts + Duration::from_nanos(frames as u64 * 1_000_000_000 / rate.max(1) as u64);
    if end <= expected {
        Align::Skip
    } else if pts + TOLERANCE < expected {
        Align::Drop(frames_between(pts, expected, rate))
    } else if pts > expected + TOLERANCE {
        Align::Silence(frames_between(expected, pts, rate))
    } else {
        Align::Keep
    }
}

pub(super) struct AudioLoop {
    decoder: Box<dyn AudioDecoder>,
    producer: rtrb::Producer<f32>,
    out: Arc<OutputShared>,
    resampler: Option<Resampler>,
    generation: u64,
    /// Where the next sample belongs on the timeline (the seek target right after a seek).
    /// Buffers are trimmed or preceded by silence to match it, so gaps, overlaps and skipped
    /// packets never shift audio against video.
    expected: Duration,
    /// Timestamp of the previous buffer. Frames of one laced Matroska block without a
    /// DefaultDuration all carry the block's timestamp; they continue the timeline as-is.
    last_pts: Option<Duration>,
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
            expected: Duration::ZERO,
            last_pts: None,
            pushed: 0,
            errors: 0,
        }
    }

    pub fn run(mut self, shared: &Shared, rx: Receiver<Msg>) {
        loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return;
            }
            if self.out.failed.load(Ordering::Relaxed) {
                shared.disable_audio("audio device lost");
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
                    self.expected = target;
                    self.last_pts = None;
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

    /// Returns `false` only on shutdown or device loss.
    fn play(&mut self, shared: &Shared, buf: AudioBuffer) -> bool {
        let ch = self.out.channels;
        let rate = buf.rate.max(1);
        let mut samples = remix(&buf.samples, buf.channels, ch);
        let same_block = self.last_pts == Some(buf.pts);
        self.last_pts = Some(buf.pts);
        let alignment = if same_block { Align::Keep } else { align(self.expected, buf.pts, buf.frames(), rate) };
        let start = match alignment {
            Align::Keep if same_block => self.expected,
            Align::Skip => return true,
            Align::Keep => buf.pts,
            Align::Drop(n) => {
                samples.drain(..(n * ch as usize).min(samples.len()));
                self.expected
            }
            Align::Silence(n) => {
                samples.splice(0..0, std::iter::repeat_n(0.0, n * ch as usize));
                self.expected
            }
        };
        let frames = samples.len() / ch.max(1) as usize;
        self.expected = start + Duration::from_nanos(frames as u64 * 1_000_000_000 / rate as u64);
        if self.resampler.as_ref().is_none_or(|r| r.rates() != (rate, self.out.rate)) {
            // The stream rate changed (or first buffer): finish the old resampler's tail first.
            if let Some(mut old) = self.resampler.take() {
                let tail = old.flush();
                if !self.push(shared, &tail) {
                    return false;
                }
            }
            match Resampler::new(rate, self.out.rate, ch) {
                Ok(r) => self.resampler = Some(r),
                Err(e) => {
                    shared.disable_audio(&format!("cannot resample {rate} Hz to {} Hz: {e}", self.out.rate));
                    return false;
                }
            }
        }
        let samples = self.resampler.as_mut().unwrap().process(&samples);
        self.push(shared, &samples)
    }

    /// Pushes resampled samples into the ring. Returns `false` on shutdown or device loss.
    fn push(&mut self, shared: &Shared, samples: &[f32]) -> bool {
        let generation = self.generation;
        let out = self.out.clone();
        let written = push_frames(&mut self.producer, &self.out, samples, || {
            !shared.shutdown.load(Ordering::SeqCst)
                && !out.failed.load(Ordering::Relaxed)
                && shared.generation.load(Ordering::SeqCst) == generation
        });
        self.pushed += written as u64;
        if shared.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        if self.out.failed.load(Ordering::Relaxed) {
            shared.disable_audio("audio device lost");
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
        // The resampler still holds the last few milliseconds.
        if let Some(r) = self.resampler.as_mut() {
            let tail = r.flush();
            if !self.push(shared, &tail) {
                return false;
            }
        }
        let capacity = self.producer.buffer().capacity();
        loop {
            if shared.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            if shared.generation.load(Ordering::SeqCst) != self.generation {
                return true;
            }
            if self.out.failed.load(Ordering::Relaxed) {
                shared.disable_audio("audio device lost");
                return false;
            }
            if self.producer.slots() == capacity {
                // Everything was heard: time goes on without audio (e.g. video still playing).
                self.out.exhausted.store(true, Ordering::SeqCst);
                shared.stream_finished(self.generation, false);
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_buffers_to_the_expected_timeline() {
        let ms = Duration::from_millis;
        // 480 frames at 48 kHz = 10 ms buffers.
        assert_eq!(align(ms(100), ms(100), 480, 48_000), Align::Keep);
        assert_eq!(align(ms(100), ms(103), 480, 48_000), Align::Keep, "timestamp jitter is ignored");
        assert_eq!(align(ms(100), ms(120), 480, 48_000), Align::Silence(960), "a 20 ms gap");
        assert_eq!(align(ms(100), ms(95), 480, 48_000), Align::Drop(240), "a 5 ms overlap");
        assert_eq!(align(ms(100), ms(80), 480, 48_000), Align::Skip, "entirely in the past");
    }
}
