//! Pitch-preserving time-scale modification by WSOLA.
//!
//! The method follows the idea of Verhelst & Roelands, "An overlap-add technique
//! based on waveform similarity (WSOLA) for high quality time-scale modification
//! of speech" (ICASSP 1993). The output is built from windows of `N` frames laid
//! down at a fixed synthesis hop of `N/2`, so neighbouring windows overlap by 50%
//! and a periodic Hann window sums to exactly one. For output window `k` the input
//! is read *nominally* at `k * hop * speed`; WSOLA lets the real read position wander
//! within a small tolerance around that point and picks the spot whose start best
//! resembles the *natural continuation* of the previously used segment (the input
//! that followed it in the original signal). Overlapping parts are therefore close
//! to in phase, so the crossfade neither cancels nor clicks, and since whole
//! waveform pieces are copied the pitch stays unchanged.
//!
//! Details of this implementation:
//! * Similarity is normalised cross-correlation `<template, candidate> / |candidate|`
//!   (the template norm is the same for every candidate and is left out). It is
//!   computed on a mono downmix, first on a coarse grid of offsets and then at every
//!   offset around the best coarse hit. Candidate energies come from a sliding sum.
//! * Ties, silence and a nominal candidate that is already a near-perfect
//!   continuation all resolve to the nominal position, which makes speed 1.0
//!   reproduce the input exactly with **zero latency**: the stream is primed with
//!   `hop` zeros and the first `hop` output frames (pure fade-in) are discarded.
//! * The nominal read position is an integer plus a fractional accumulator, so
//!   output frame `t` always corresponds to input frame `t * speed` (to within
//!   the search tolerance) no matter how long the stream runs or how it is chunked.

/// Window length in milliseconds (rounded to an even number of frames).
const WINDOW_MS: f64 = 20.0;
/// Half-width of the similarity search in milliseconds (±6 ms).
const SEARCH_MS: f64 = 6.0;
/// Spacing of the coarse search grid in milliseconds (6 frames at 48 kHz).
const COARSE_MS: f64 = 0.125;
/// Speed limits (dimensionless playback-rate factor).
const MIN_SPEED: f64 = 0.25;
const MAX_SPEED: f64 = 4.0;
/// Maximum supported channel count.
const MAX_CHANNELS: usize = 8;
/// Margin, as a fraction of the template norm, a candidate must beat the current best by.
const TIE_EPS: f64 = 1e-6;
/// If the nominal candidate reaches this fraction of the best possible score it is kept as is.
const PERFECT: f64 = 1.0 - 1e-4;
/// Candidate energy (sum of squares) below which a candidate counts as silent.
const SILENT: f64 = 1e-20;

/// Streaming WSOLA time stretcher for interleaved `f32` audio.
pub struct TimeStretcher {
    channels: usize,
    speed: f64,
    /// Window length `N` and synthesis hop `N/2`, in frames.
    win: usize,
    hop: usize,
    /// Search half-width and coarse grid step, in frames (`search` is a multiple of `coarse`).
    search: usize,
    coarse: usize,
    window: Vec<f32>,
    /// Buffered input (interleaved) and its mono downmix; index 0 is `discarded` frames into the
    /// primed stream (which starts with `hop` zeros).
    input: Vec<f32>,
    mono: Vec<f32>,
    /// Scratch: energy of each candidate's overlap region, one entry per search offset.
    energy: Vec<f64>,
    /// Overlap-add accumulator holding one window of output (interleaved).
    ola: Vec<f32>,
    /// Incomplete trailing frame from the last `process` call.
    pending: [f32; MAX_CHANNELS],
    pending_len: usize,
    started: bool,
    /// Nominal read position of the next window: `nominal + frac`, in buffer frames.
    nominal: usize,
    frac: f64,
    /// Where the previous window was actually read.
    prev_pos: usize,
    discarded: u64,
}

fn sanitize(speed: f64) -> f64 {
    if speed.is_finite() {
        speed.clamp(MIN_SPEED, MAX_SPEED)
    } else {
        1.0
    }
}

/// Dot product with eight independent accumulators (lets the compiler vectorise).
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    let mut acc = [0f32; 8];
    for (x, y) in ca.iter().zip(cb) {
        for ((s, p), q) in acc.iter_mut().zip(x).zip(y) {
            *s += p * q;
        }
    }
    acc.iter().sum::<f32>() + tail
}

impl TimeStretcher {
    pub fn new(channels: u16, rate: u32, speed: f64) -> Self {
        let channels = usize::from(channels).clamp(1, MAX_CHANNELS);
        let rate = f64::from(rate.clamp(8_000, 192_000));
        let hop = (rate * WINDOW_MS / 2000.0).round() as usize;
        let coarse = ((rate * COARSE_MS / 1000.0).round() as usize).max(1);
        let search = (rate * SEARCH_MS / 1000.0 / coarse as f64).round() as usize * coarse;
        let mut window = vec![0f32; 2 * hop];
        for j in 0..hop {
            let w = (0.5 - 0.5 * (std::f64::consts::TAU * j as f64 / (2 * hop) as f64).cos()) as f32;
            window[j] = w;
            window[j + hop] = 1.0 - w; // periodic Hann: w[j] + w[j + N/2] == 1
        }
        let mut s = TimeStretcher {
            channels,
            speed: sanitize(speed),
            win: 2 * hop,
            hop,
            search,
            coarse,
            window,
            input: Vec::new(),
            mono: Vec::new(),
            energy: vec![0.0; 2 * search + 1],
            ola: vec![0.0; 2 * hop * channels],
            pending: [0.0; MAX_CHANNELS],
            pending_len: 0,
            started: false,
            nominal: 0,
            frac: 0.0,
            prev_pos: 0,
            discarded: 0,
        };
        s.reset();
        s
    }

    /// Change the speed; takes effect from the next window, without resetting anything.
    pub fn set_speed(&mut self, speed: f64) {
        self.speed = sanitize(speed);
    }

    /// Output frames still to come that belong to the speed in force *before* a `set_speed`
    /// made right now. Always 0 here: the speed is read at the start of each window step, and
    /// that step both advances the nominal input position by `hop * speed` and emits the `hop`
    /// output frames that span exactly that advance. No finished-but-unemitted output exists,
    /// so every frame emitted after `set_speed` maps to input at the new speed (output frame
    /// `t` after the change covers input `consumed_at_change + t * new_speed`).
    pub fn frames_before_speed_change(&self) -> usize {
        0
    }

    /// Drop all buffered audio and state (e.g. after a seek). The speed is kept.
    pub fn reset(&mut self) {
        self.input.clear();
        self.input.resize(self.hop * self.channels, 0.0);
        self.mono.clear();
        self.mono.resize(self.hop, 0.0);
        self.ola.fill(0.0);
        self.pending_len = 0;
        self.started = false;
        self.nominal = 0;
        self.frac = 0.0;
        self.prev_pos = 0;
        self.discarded = 0;
    }

    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        self.process_into(samples, &mut out);
        out
    }

    pub fn flush(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        self.flush_into(&mut out);
        out
    }

    /// Feed interleaved samples and append any finished output to `out`. A trailing partial
    /// frame is kept until the next call.
    pub fn process_into(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let mut rest = samples;
        if self.pending_len > 0 {
            let take = (ch - self.pending_len).min(rest.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&rest[..take]);
            self.pending_len += take;
            rest = &rest[take..];
            if self.pending_len < ch {
                return;
            }
            let frame = self.pending;
            self.push_frame(&frame[..ch]);
            self.pending_len = 0;
        }
        let mut frames = rest.chunks_exact(ch);
        for f in &mut frames {
            self.push_frame(f);
        }
        let tail = frames.remainder();
        self.pending[..tail.len()].copy_from_slice(tail);
        self.pending_len = tail.len();
        while self.ready() {
            self.step(out, usize::MAX);
        }
        self.compact();
    }

    /// End of stream: pad with silence and emit output until all input has been covered, so the
    /// stream's total output is `input / speed` frames. The stretcher is reset afterwards.
    pub fn flush_into(&mut self, out: &mut Vec<f32>) {
        let fed = (self.discarded + self.mono.len() as u64) as f64 - self.hop as f64;
        let mut remaining = ((fed - self.consumed_frames()).max(0.0) / self.speed).round() as usize;
        while remaining > 0 {
            let need = if self.started { self.nominal + self.search + self.win } else { self.win };
            if self.mono.len() < need {
                self.mono.resize(need, 0.0);
                self.input.resize(need * self.channels, 0.0);
            }
            remaining -= self.step(out, remaining);
        }
        self.reset();
    }

    /// Input frames (real stream position) that the next window will nominally start at; equal
    /// to `output_frames * speed` at constant speed.
    fn consumed_frames(&self) -> f64 {
        if !self.started {
            return 0.0;
        }
        (self.discarded + self.nominal as u64) as f64 + self.frac - self.hop as f64
    }

    fn push_frame(&mut self, frame: &[f32]) {
        self.input.extend_from_slice(frame);
        self.mono.push(frame.iter().sum());
    }

    fn ready(&self) -> bool {
        let need = if self.started { self.nominal + self.search + self.win } else { self.win };
        self.mono.len() >= need
    }

    /// Place one window and emit up to `limit` finished frames; returns the number emitted.
    fn step(&mut self, out: &mut Vec<f32>, limit: usize) -> usize {
        let (h, ch) = (self.hop, self.channels);
        let first = !self.started;
        let pos = if first {
            self.started = true;
            self.nominal = h;
            0
        } else {
            let p = self.find_best();
            self.frac += h as f64 * self.speed;
            let whole = self.frac.floor();
            self.frac -= whole;
            self.nominal += whole as usize;
            p
        };
        self.prev_pos = pos;
        let seg = &self.input[pos * ch..(pos + self.win) * ch];
        for ((o, x), &w) in self.ola.chunks_exact_mut(ch).zip(seg.chunks_exact(ch)).zip(&self.window) {
            for (a, &b) in o.iter_mut().zip(x) {
                *a += b * w;
            }
        }
        // The first window's leading half only fades in over the priming zeros: drop it.
        let n = if first { 0 } else { h.min(limit) };
        out.extend_from_slice(&self.ola[..n * ch]);
        self.ola.copy_within(h * ch.., 0);
        self.ola[h * ch..].fill(0.0);
        n
    }

    /// WSOLA search around `self.nominal`; returns the chosen read position.
    fn find_best(&mut self) -> usize {
        let (h, s, c) = (self.hop, self.search, self.coarse);
        let span = 2 * s;
        let lo = self.nominal - s;
        let tpl = &self.mono[self.prev_pos + h..self.prev_pos + 2 * h];
        let region = &self.mono[lo..lo + span + h];
        let norm = tpl.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>().sqrt();

        let mut e: f64 = region[..h].iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        for (k, slot) in self.energy.iter_mut().enumerate() {
            *slot = e;
            if k < span {
                e += f64::from(region[k + h]).powi(2) - f64::from(region[k]).powi(2);
            }
        }
        let energy = &self.energy;
        let score = |off: usize| -> f64 {
            let en = energy[off];
            if en > SILENT {
                f64::from(dot(tpl, &region[off..off + h])) / en.sqrt()
            } else {
                0.0
            }
        };

        let (mut best, mut best_score) = (s, score(s));
        if best_score >= PERFECT * norm {
            return self.nominal;
        }
        let eps = TIE_EPS * norm;
        for off in (0..=span).step_by(c).filter(|&o| o != s) {
            let sc = score(off);
            if sc > best_score + eps {
                (best, best_score) = (off, sc);
            }
        }
        let centre = best;
        for off in centre.saturating_sub(c - 1)..=(centre + c - 1).min(span) {
            let sc = score(off);
            if off != centre && sc > best_score + eps {
                (best, best_score) = (off, sc);
            }
        }
        lo + best
    }

    /// Drop input that no future window or template can reach (amortised: only in large steps).
    fn compact(&mut self) {
        if !self.started {
            return;
        }
        let keep = self.prev_pos.min(self.nominal - self.search);
        if keep < 4 * self.win {
            return;
        }
        self.mono.copy_within(keep.., 0);
        self.mono.truncate(self.mono.len() - keep);
        self.input.copy_within(keep * self.channels.., 0);
        self.input.truncate(self.input.len() - keep * self.channels);
        self.nominal -= keep;
        self.prev_pos -= keep;
        self.discarded += keep as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const WIN: usize = 960; // window at 48 kHz, in frames

    fn sine(ch: usize, frames: usize, freq: f64, amp: f32) -> Vec<f32> {
        (0..frames * ch)
            .map(|i| amp * (std::f64::consts::TAU * freq * (i / ch) as f64 / f64::from(RATE)).sin() as f32)
            .collect()
    }

    fn run(ts: &mut TimeStretcher, input: &[f32], chunks: &[usize]) -> Vec<f32> {
        let (mut out, mut i, mut k) = (Vec::new(), 0, 0);
        while i < input.len() {
            let n = chunks[k % chunks.len()].min(input.len() - i);
            ts.process_into(&input[i..i + n], &mut out);
            i += n;
            k += 1;
        }
        ts.flush_into(&mut out);
        out
    }

    /// Largest same-channel sample-to-sample step, ignoring the last window (end-of-input edge).
    fn max_step(out: &[f32], ch: usize) -> f32 {
        let end = out.len().saturating_sub(WIN * ch);
        out[..end].windows(ch + 1).map(|w| (w[ch] - w[0]).abs()).fold(0.0, f32::max)
    }

    const SINE_STEP: f32 = 0.5 * std::f32::consts::TAU * 440.0 / 48_000.0;

    #[test]
    fn duration_matches_speed() {
        let input = sine(2, 48_000, 440.0, 0.5);
        for speed in [0.25, 0.5, 1.5, 2.0, 4.0] {
            let out = run(&mut TimeStretcher::new(2, RATE, speed), &input, &[48_000 * 2]);
            let (got, want) = ((out.len() / 2) as f64, 48_000.0 / speed);
            assert!((got - want).abs() / want < 0.02, "speed {speed}: {got} vs {want}");
        }
    }

    #[test]
    fn chunking_does_not_change_length() {
        let input = sine(2, 48_000, 440.0, 0.5);
        for speed in [4.0, 0.5] {
            let one = run(&mut TimeStretcher::new(2, RATE, speed), &input, &[input.len()]);
            let many = run(&mut TimeStretcher::new(2, RATE, speed), &input, &[1, 7, 777, 4096, 3, 1000]);
            assert!(one.len().abs_diff(many.len()) <= WIN * 2, "{} vs {}", one.len(), many.len());
        }
    }

    #[test]
    fn unity_speed_is_transparent() {
        let mut seed = 12345u32;
        let input: Vec<f32> = (0..2 * 48_000)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let t = (i / 2) as f32 / 48_000.0;
                0.3 * (t * 2765.0).sin() + 0.2 * (t * 700.0).cos() + (seed >> 8) as f32 / 16_777_216.0 * 0.2 - 0.1
            })
            .collect();
        for chunks in [&[input.len()][..], &[1, 7, 777, 4096]] {
            let out = run(&mut TimeStretcher::new(2, RATE, 1.0), &input, chunks);
            assert_eq!(out.len(), input.len());
            let err = out.iter().zip(&input).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
            assert!(err < 1e-5, "max error {err}");
        }
    }

    #[test]
    fn no_clicks_on_sine() {
        let input = sine(2, 48_000, 440.0, 0.5);
        for speed in [2.0, 0.5] {
            let out = run(&mut TimeStretcher::new(2, RATE, speed), &input, &[4096]);
            let step = max_step(&out, 2);
            assert!(step < 2.0 * SINE_STEP, "speed {speed}: step {step}");
        }
    }

    #[test]
    fn no_drift_over_a_minute() {
        let input = sine(2, 60 * 48_000, 440.0, 0.5);
        let mut ts = TimeStretcher::new(2, RATE, 1.5);
        let mut out = Vec::new();
        for c in input.chunks(2000) {
            ts.process_into(c, &mut out);
        }
        let err = ts.consumed_frames() - (out.len() / 2) as f64 * 1.5;
        assert!(err.abs() < WIN as f64, "drift {err}");
    }

    #[test]
    fn speed_boundary_attribution() {
        let input = sine(2, 20 * 48_000, 440.0, 0.5);
        let (first, second) = input.split_at(input.len() / 2);
        let mut ts = TimeStretcher::new(2, RATE, 1.0);
        let mut out = Vec::new();
        first.chunks(2000).for_each(|c| ts.process_into(c, &mut out));
        let before = out.len() / 2 + ts.frames_before_speed_change();
        ts.set_speed(4.0);
        second.chunks(2000).for_each(|c| ts.process_into(c, &mut out));
        let attributed = |out: &[f32]| before as f64 + (out.len() / 2 - before) as f64 * 4.0;
        let err = attributed(&out) - ts.consumed_frames();
        assert!(err.abs() < WIN as f64, "mid-stream error {err}");
        ts.flush_into(&mut out);
        let err = attributed(&out) - (20 * 48_000) as f64;
        assert!(err.abs() < WIN as f64, "end-of-stream error {err}");
    }

    #[test]
    fn speed_changes_mid_stream() {
        let input = sine(2, 3 * 48_000, 440.0, 0.5);
        let mut ts = TimeStretcher::new(2, RATE, 1.0);
        let mut out = Vec::new();
        for (i, c) in input.chunks(2 * 14_400).enumerate() {
            ts.set_speed([1.0, 3.0, 0.5][i % 3]);
            ts.process_into(c, &mut out);
        }
        ts.flush_into(&mut out);
        let step = max_step(&out, 2);
        assert!(step < 2.0 * SINE_STEP, "step {step}");
    }

    #[test]
    fn mono_multichannel_and_silence() {
        for ch in [1usize, 6] {
            let input = sine(ch, 48_000, 440.0, 0.5);
            let out = run(&mut TimeStretcher::new(ch as u16, RATE, 1.5), &input, &[333]);
            let frames = (out.len() / ch) as f64;
            assert!((frames - 32_000.0).abs() < WIN as f64 && out.iter().all(|x| x.is_finite()));
        }
        let out = run(&mut TimeStretcher::new(2, RATE, 2.0), &vec![0.0; 96_000], &[1, 4096]);
        assert!(!out.is_empty() && out.iter().all(|&x| x == 0.0));
        // Degenerate calls must not panic.
        let mut ts = TimeStretcher::new(0, 0, f64::NAN);
        assert!(ts.process(&[]).is_empty() && ts.flush().is_empty());
        ts.set_speed(100.0);
        ts.process(&[0.1; 3]);
        ts.reset();
        ts.flush();
    }

    #[test]
    #[ignore]
    fn throughput() {
        let input = sine(2, 30 * 48_000, 440.0, 0.5);
        let mut ts = TimeStretcher::new(2, RATE, 2.0);
        let mut out = Vec::with_capacity(input.len());
        let t = std::time::Instant::now();
        for c in input.chunks(4096) {
            ts.process_into(c, &mut out);
        }
        let secs = t.elapsed().as_secs_f64();
        println!("throughput at 2.0x: {:.0} s of 48 kHz stereo per CPU second", 30.0 / secs);
    }
}
