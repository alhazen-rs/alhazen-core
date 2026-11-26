//! Time-stretching with pitch preservation (WSOLA — waveform-similarity
//! overlap-add, the same family as mpv's scaletempo): audio at 0.5x–4x speed
//! keeps its pitch, so fast review playback stays intelligible.
//!
//! Each step takes a `win`-frame input window; the output advances by `hop`
//! frames (= win/2). The new window's first half is crossfaded against the
//! previous window's second half — and the window's start is refined within
//! ±search by cross-correlation so the two halves are phase-aligned, which is
//! what keeps the waveform continuous instead of clicky. The input position
//! then advances `hop × speed` frames.

/// Window length in milliseconds (18.7 ms ≈ 900 frames at 48 kHz).
const WIN_MS: f64 = 18.7;

pub struct TimeStretcher {
    channels: usize,
    speed: f64,
    win: usize,
    hop: usize,
    search: usize,
    /// Pending input samples, interleaved; `head` frames at the front are consumed.
    input: Vec<f32>,
    /// Consumed input length in frames.
    head: usize,
    /// The previous step's input window (interleaved, `win` frames).
    prev: Vec<f32>,
    /// Fractional position (frames, relative to `head`) where the next window starts.
    in_pos: f64,
    primed: bool,
}

impl TimeStretcher {
    pub fn new(channels: u16, rate: u32, speed: f64) -> Self {
        let win = ((rate as f64 * WIN_MS / 1000.0) as usize) & !1;
        Self {
            channels: channels as usize,
            speed,
            win,
            hop: win / 2,
            search: win / 4,
            input: Vec::new(),
            head: 0,
            prev: Vec::new(),
            in_pos: 0.0,
            primed: false,
        }
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed;
    }

    /// Resets all state (after a seek or a speed change).
    pub fn reset(&mut self) {
        self.input.clear();
        self.head = 0;
        self.prev.clear();
        self.in_pos = 0.0;
        self.primed = false;
    }

    #[inline]
    fn at(&self, frame: usize) -> &[f32] {
        let ch = self.channels;
        let start = (self.head + frame) * ch;
        &self.input[start..start + ch]
    }

    #[inline]
    fn avail(&self) -> usize {
        self.input.len() / self.channels - self.head
    }

    /// Cross-correlation of the candidate window's first `hop` frames with the
    /// previous window's second half, over all channels at once.
    fn correlation(&self, off: usize) -> f64 {
        let ch = self.channels;
        let mut corr = 0.0f64;
        // Stride by 8 samples: the peak position barely moves, the cost drops 8x.
        let stride = 8;
        let tail = &self.prev[self.hop * ch..];
        let base = (self.head + off) * ch;
        let mut i = 0;
        while i < self.hop * ch {
            corr += f64::from(self.input[base + i]) * f64::from(tail[i]);
            i += stride;
        }
        corr
    }

    /// Feeds interleaved input; returns interleaved stretched output.
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        self.input.extend_from_slice(samples);
        self.produce(false)
    }

    /// End of stream: pad with silence and drain what can still be formed.
    pub fn flush(&mut self) -> Vec<f32> {
        let pad = (self.win + self.search + self.hop) * self.channels;
        let n = self.input.len();
        self.input.resize(n + pad, 0.0);
        self.produce(true)
    }

    fn produce(&mut self, flushing: bool) -> Vec<f32> {
        let ch = self.channels;
        let mut out = Vec::new();
        loop {
            let base = self.in_pos.floor() as usize;
            let avail = self.avail();
            if avail < base + self.search + self.win {
                let enough = avail >= base + self.win;
                if !flushing || !enough {
                    break;
                }
            }
            if !self.primed {
                // The first window's first half goes out verbatim; nothing to crossfade yet.
                let mut w = Vec::with_capacity(self.win * ch);
                for f in 0..self.win {
                    if base + f < avail {
                        w.extend_from_slice(self.at(base + f));
                    } else {
                        w.extend_from_slice(&vec![0.0; ch]);
                    }
                }
                out.extend_from_slice(&w[..self.hop * ch]);
                self.prev = w;
                self.primed = true;
                self.advance();
                continue;
            }
            // Refine the window start by correlation against the previous tail.
            let lo = base.saturating_sub(self.search);
            let hi = (base + self.search).min(avail.saturating_sub(self.win));
            let mut best = base.min(hi);
            let mut best_corr = f64::MIN;
            let mut off = lo;
            while off <= hi {
                let c = self.correlation(off);
                if c > best_corr {
                    best_corr = c;
                    best = off;
                }
                off += 1;
            }
            // Emit the crossfade of the previous window's second half with the
            // new window's first half; the new window becomes `prev`.
            let mut w = Vec::with_capacity(self.win * ch);
            for f in 0..self.win {
                w.extend_from_slice(self.at(best + f));
            }
            for i in 0..self.hop * ch {
                let frame = i / ch;
                let t = frame as f32 / self.hop as f32;
                let old = self.prev[self.hop * ch + i];
                out.push(old * (1.0 - t) + w[i] * t);
            }
            self.prev = w;
            self.advance();
        }
        // Compact the consumed input once in a while.
        if self.head > 4096 {
            self.input.drain(..self.head * ch);
            self.head = 0;
        }
        out
    }

    fn advance(&mut self) {
        self.in_pos += self.hop as f64 * self.speed;
        // Drop input no future window can start at.
        let drop = (self.in_pos.floor() as usize).saturating_sub(self.search);
        if drop > 0 {
            self.head += drop;
            self.in_pos -= drop as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (i as f64 * freq * 2.0 * std::f64::consts::PI / rate as f64).sin() as f32;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn output_duration_scales_by_speed() {
        let rate = 48_000u32;
        let input = sine(rate, 440.0, rate as usize); // 1 s stereo
        for speed in [0.5, 1.5, 2.0] {
            let mut ts = TimeStretcher::new(2, rate, speed);
            let mut out = ts.process(&input);
            out.extend(ts.flush());
            let frames = out.len() / 2;
            let expect = rate as f64 / speed;
            let err = (frames as f64 - expect).abs() / expect;
            assert!(err < 0.05, "speed {speed}: {frames} frames vs {expect}");
        }
    }

    #[test]
    fn sine_stays_continuous_no_clicks() {
        let rate = 48_000u32;
        let input = sine(rate, 440.0, rate as usize);
        let mut ts = TimeStretcher::new(2, rate, 2.0);
        let mut out = ts.process(&input);
        out.extend(ts.flush());
        // A click is a sample-to-sample jump far larger than a 440 Hz sine ever makes.
        let max_step = (4.0 * 440.0 / rate as f64) as f32 * 4.0;
        let ch = 2;
        let frames: Vec<&[f32]> = out.chunks_exact(ch).collect();
        for w in frames.windows(2) {
            let d = (w[1][0] - w[0][0]).abs();
            assert!(d < max_step, "click in output: step {d}");
        }
    }
}
