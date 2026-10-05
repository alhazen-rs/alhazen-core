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
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: u16) -> Self {
        let mut r = Self { from, to, channels: channels.max(1) as usize, inner: None, pending: Vec::new(), skip: 0 };
        r.reset();
        r
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.inner = (self.from != self.to)
            .then(|| Fft::new(self.from as usize, self.to as usize, CHUNK, self.channels, FixedSync::Input).ok())
            .flatten();
        self.skip = self.inner.as_ref().map(|r| r.output_delay()).unwrap_or(0);
    }

    /// Feeds interleaved samples; returns whatever output is ready (may be empty).
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let Some(inner) = self.inner.as_mut() else {
            return samples.to_vec();
        };
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        let ch = self.channels;
        while self.pending.len() >= CHUNK * ch {
            let input = InterleavedSlice::new(&self.pending[..CHUNK * ch], ch, CHUNK).expect("sized input");
            match inner.process(&input, None) {
                Ok(buf) => out.extend(buf.take_data()),
                Err(e) => log::warn!("resampler: {e}"),
            }
            self.pending.drain(..CHUNK * ch);
        }
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
        let mut r = Resampler::new(48_000, 48_000, 2);
        assert_eq!(r.process(&[0.1, 0.2]), vec![0.1, 0.2]);
    }

    #[test]
    fn converts_44k1_to_48k_keeping_pitch() {
        let mut r = Resampler::new(44_100, 48_000, 1);
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
        let mut r = Resampler::new(44_100, 48_000, 1);
        r.process(&[0.5; 500]);
        r.reset();
        assert!(r.process(&[0.0; 10]).is_empty());
    }
}
