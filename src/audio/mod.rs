//! Audio output, the audio-driven clock, resampling and channel mixing.

mod clock;
mod mix;
mod output;
mod resample;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub use clock::AudioClock;
pub(crate) use mix::remix;
pub use output::NullOutput;
pub use output::AudioOutputConfig;
pub(crate) use output::{OutputShared, open_output, push_frames};
pub(crate) use resample::Resampler;

/// Whether this build can play sound on a real device (the `audio-output` feature).
pub fn output_available() -> bool {
    cfg!(feature = "audio-output")
}

/// Volume and mute, shared with the output callback (applied per sample, so changes are instant).
#[derive(Debug)]
pub struct Volume {
    gain: AtomicU32,
    muted: AtomicBool,
}

impl Default for Volume {
    fn default() -> Self {
        Self { gain: AtomicU32::new(1.0f32.to_bits()), muted: AtomicBool::new(false) }
    }
}

impl Volume {
    /// Clamped to 0.0..=1.0.
    pub fn set(&self, gain: f32) {
        let gain = if gain.is_nan() { 0.0 } else { gain.clamp(0.0, 1.0) };
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
    /// The factor actually applied to samples.
    pub fn effective(&self) -> f32 {
        if self.is_muted() { 0.0 } else { self.get() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_clamps_and_mutes() {
        let v = Volume::default();
        assert_eq!(v.effective(), 1.0);
        v.set(1.7);
        assert_eq!(v.get(), 1.0);
        v.set(-1.0);
        assert_eq!(v.get(), 0.0);
        v.set(f32::NAN);
        assert_eq!(v.get(), 0.0);
        v.set(0.5);
        v.set_muted(true);
        assert_eq!(v.effective(), 0.0);
        v.set_muted(false);
        assert_eq!(v.effective(), 0.5);
    }
}
