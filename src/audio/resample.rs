//! Sample-rate conversion (stream rate -> device rate) with rubato.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler as _};

const CHUNK: usize = 1024;

/// Streaming resampler for interleaved f32. A no-op when rates match.
pub(crate) struct Resampler {
    from: u32,
    to: u32,
    channels: usize,
    inner: Option<Fft<f32>>,
    pending: Vec<f32>,
    /// Output frames still to drop: the resampler's own delay after a (re)start.
    skip: usize,
    /// Frames fed / emitted since the last reset, so `flush` knows how much output is owed.
    frames_in: u64,
    frames_out: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: u16) -> Result<Self, String> {
        if from == 0 || to == 0 {
            return Err(format!("invalid sample rates {from} -> {to}"));
        }
        let channels = channels.max(1) as usize;
        let inner = if from == to {
            None
        } else {
            Some(Fft::new(from as usize, to as usize, CHUNK, channels, FixedSync::Input).map_err(|e| e.to_string())?)
        };
        let mut r = Self { from, to, channels, inner, pending: Vec::new(), skip: 0, frames_in: 0, frames_out: 0 };
        r.reset();
        Ok(r)
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.frames_in = 0;
        self.frames_out = 0;
        self.skip = match self.inner.as_mut() {
            Some(r) => {
                r.reset();
                r.output_delay()
            }
            None => 0,
        };
    }

    /// Feeds interleaved samples; returns whatever output is ready (may be empty).
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        if self.inner.is_none() {
            return samples.to_vec();
        }
        self.frames_in += (samples.len() / self.channels) as u64;
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= CHUNK * self.channels {
            out.extend(self.run_chunk());
        }
        self.frames_out += (out.len() / self.channels) as u64;
        out
    }

    /// Emits everything still owed (the partial last chunk and the filter tail) and resets.
    pub fn flush(&mut self) -> Vec<f32> {
        if self.inner.is_none() || self.frames_in == 0 {
            self.reset();
            return Vec::new();
        }
        let owed = (self.frames_in as u128 * self.to as u128 + self.from as u128 / 2) / self.from as u128;
        let owed = owed as u64 - self.frames_out.min(owed as u64);
        let mut out = Vec::new();
        // Zero-pad until the real tail has come out; a few chunks always suffice.
        for _ in 0..4 {
            if (out.len() / self.channels) as u64 >= owed {
                break;
            }
            self.pending.resize(CHUNK * self.channels, 0.0);
            out.extend(self.run_chunk());
        }
        out.truncate(owed as usize * self.channels);
        self.reset();
        out
    }

    /// Resamples one CHUNK from `pending`, dropping the start-up delay.
    fn run_chunk(&mut self) -> Vec<f32> {
        let ch = self.channels;
        let inner = self.inner.as_mut().expect("resampling");
        let input = InterleavedSlice::new(&self.pending[..CHUNK * ch], ch, CHUNK).expect("sized input");
        let mut out = match inner.process(&input, None) {
            Ok(buf) => buf.take_data(),
            Err(e) => {
                log::warn!("resampler: {e}");
                Vec::new()
            }
        };
        self.pending.drain(..CHUNK * ch);
        let drop = (self.skip * ch).min(out.len());
        self.skip -= drop / ch;
        out.drain(..drop);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames).map(|i| (i as f32 * freq * std::f32::consts::TAU / rate as f32).sin() * 0.5).collect()
    }

    fn crossings(s: &[f32]) -> usize {
        s.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
    }

    #[test]
    fn same_rate_is_passthrough() {
        let mut r = Resampler::new(48_000, 48_000, 2).unwrap();
        assert_eq!(r.process(&[0.1, 0.2]), vec![0.1, 0.2]);
    }

    #[test]
    fn converts_44k1_to_48k_keeping_pitch() {
        let mut r = Resampler::new(44_100, 48_000, 1).unwrap();
        let input = sine(1_000.0, 44_100, 44_100);
        let out: Vec<f32> = input.chunks(441).flat_map(|c| r.process(c)).collect();
        // One second in -> about one second out (minus the last partial chunk and the delay).
        assert!((45_000..=48_000).contains(&out.len()), "got {}", out.len());
        let seconds = out.len() as f32 / 48_000.0;
        let hz = crossings(&out) as f32 / 2.0 / seconds;
        assert!((990.0..=1010.0).contains(&hz), "pitch {hz} Hz");
    }

    #[test]
    fn reset_drops_pending_input() {
        let mut r = Resampler::new(44_100, 48_000, 1).unwrap();
        r.process(&[0.5; 500]);
        r.reset();
        assert!(r.process(&[0.0; 10]).is_empty());
    }

    #[test]
    fn flush_returns_the_tail_so_no_audio_is_lost() {
        let mut r = Resampler::new(44_100, 48_000, 1).unwrap();
        let input = sine(1_000.0, 44_100, 1_000);
        let mut out = r.process(&input);
        out.extend(r.flush());
        // 1000 frames at 44.1 kHz = 1088.4 frames at 48 kHz.
        assert!((1_087..=1_089).contains(&out.len()), "got {} frames", out.len());
        assert!(r.process(&[]).is_empty() && r.flush().is_empty(), "flush leaves a clean state");
    }

    #[test]
    fn same_rate_flush_is_empty() {
        let mut r = Resampler::new(48_000, 48_000, 2).unwrap();
        assert_eq!(r.process(&[0.1, 0.2]), vec![0.1, 0.2]);
        assert!(r.flush().is_empty());
    }

    #[test]
    fn invalid_rates_are_an_error_not_silent_passthrough() {
        assert!(Resampler::new(0, 48_000, 2).is_err());
        assert!(Resampler::new(44_100, 0, 2).is_err());
    }
}
