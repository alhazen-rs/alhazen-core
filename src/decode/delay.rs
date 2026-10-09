//! Start-up padding (encoder priming) trimming: the first `codec_delay` of decoded audio is not
//! part of the presentation (Matroska CodecDelay, MP4 edit list `media_time`).

use std::time::Duration;

use super::audio::AudioBuffer;

#[cfg_attr(not(any(feature = "native-aac", feature = "native-mp3", feature = "ffmpeg-cli", all(windows, feature = "media-foundation"))), allow(dead_code))]
pub(crate) struct DelayTrim {
    delay: Duration,
    /// Frames still to drop, at the output rate (computed when the first buffer's rate is known).
    skip: Option<usize>,
    armed: bool,
    /// The stream's exact presentation length, when known: later samples are end padding.
    end: Option<Duration>,
}

#[cfg_attr(not(any(feature = "native-aac", feature = "native-mp3", feature = "ffmpeg-cli", all(windows, feature = "media-foundation"))), allow(dead_code))]
impl DelayTrim {
    pub fn new(codec_delay: Duration) -> Self {
        Self { delay: codec_delay, skip: None, armed: true, end: None }
    }

    /// Also drops the samples presented at or after `end` (the stream's exact length: encoder end
    /// padding).
    pub fn with_end(mut self, end: Option<Duration>) -> Self {
        self.end = end;
        self
    }

    /// Call with each packet's pts before its samples: seeking back to the stream start (pts 0
    /// after a [`reset`](Self::reset)) re-arms the trim. Further pts-0 packets (laced frames of the
    /// first block) do not.
    pub fn on_packet(&mut self, pts: Duration) {
        if pts.is_zero() && !self.armed {
            self.armed = true;
            self.skip = None;
        }
    }

    /// A seek elsewhere: the samples there are not padding.
    pub fn reset(&mut self) {
        self.armed = false;
        self.skip = Some(0);
    }

    /// Drops what remains of the padding from the front of `samples` (interleaved) and returns the
    /// rest, timed on the presentation timeline; `None` when nothing is left.
    pub fn apply(&mut self, mut samples: Vec<f32>, channels: u16, rate: u32, pts: Duration) -> Option<AudioBuffer> {
        let ch = channels.max(1) as usize;
        let skip = self.skip.get_or_insert_with(|| {
            if self.armed { (self.delay.as_secs_f64() * rate as f64).round() as usize } else { 0 }
        });
        let frames = samples.len() / ch;
        let drop = (*skip).min(frames);
        *skip -= drop;
        samples.drain(..drop * ch);
        if samples.is_empty() {
            return None;
        }
        let start = pts + Duration::from_secs_f64(drop as f64 / rate.max(1) as f64);
        let pts = start.saturating_sub(self.delay);
        let samples = clip_end(samples, channels, rate, pts, self.end);
        if samples.is_empty() {
            return None;
        }
        Some(AudioBuffer { rate, channels, samples, pts })
    }
}

/// Drops the frames of `samples` (interleaved; the first presented at `pts`) presented at or after
/// `end`.
pub(crate) fn clip_end(mut samples: Vec<f32>, channels: u16, rate: u32, pts: Duration, end: Option<Duration>) -> Vec<f32> {
    if let Some(end) = end {
        let keep = (end.saturating_sub(pts).as_secs_f64() * rate as f64).round() as usize;
        samples.truncate(keep.saturating_mul(channels.max(1) as usize));
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn buf(frames: usize, ch: u16) -> Vec<f32> {
        (0..frames * ch as usize).map(|i| i as f32).collect()
    }

    #[test]
    fn drops_the_delay_across_buffers_and_shifts_pts() {
        // 1500 frames of padding at 1000 Hz, 1024-frame buffers.
        let mut t = DelayTrim::new(MS(1500));
        t.on_packet(MS(0));
        assert!(t.apply(buf(1024, 2), 2, 1000, MS(0)).is_none(), "all padding");
        t.on_packet(MS(1024));
        let b = t.apply(buf(1024, 2), 2, 1000, MS(1024)).unwrap();
        assert_eq!(b.frames(), 1024 - 476);
        assert_eq!(b.samples[0], (476 * 2) as f32, "starts after the padding");
        assert_eq!(b.pts, MS(0), "presentation starts at 0");
        t.on_packet(MS(2048));
        let b = t.apply(buf(1024, 2), 2, 1000, MS(2048)).unwrap();
        assert_eq!((b.frames(), b.pts), (1024, MS(548)));
    }

    #[test]
    fn rearms_at_stream_start_and_not_after_a_seek() {
        let mut t = DelayTrim::new(MS(10));
        t.reset(); // a seek elsewhere: no padding there
        t.on_packet(MS(500));
        let b = t.apply(buf(100, 1), 1, 1000, MS(500)).unwrap();
        assert_eq!((b.frames(), b.pts), (100, MS(490)));
        t.on_packet(MS(0)); // seek back to the start: padding again
        let b = t.apply(buf(100, 1), 1, 1000, MS(0)).unwrap();
        assert_eq!((b.frames(), b.pts), (90, MS(0)));
    }

    #[test]
    fn repeated_pts_zero_packets_trim_once() {
        // Laced Matroska frames without DefaultDuration all carry the block's pts (0).
        let mut t = DelayTrim::new(MS(10));
        t.on_packet(MS(0));
        let b = t.apply(buf(100, 1), 1, 1000, MS(0)).unwrap();
        assert_eq!(b.frames(), 90);
        t.on_packet(MS(0));
        let b = t.apply(buf(100, 1), 1, 1000, MS(0)).unwrap();
        assert_eq!(b.frames(), 100, "the second frame of the block is not padding");
    }

    #[test]
    fn no_delay_passes_everything_through() {
        let mut t = DelayTrim::new(Duration::ZERO);
        t.on_packet(MS(0));
        let b = t.apply(buf(64, 2), 2, 48_000, MS(0)).unwrap();
        assert_eq!((b.frames(), b.pts), (64, MS(0)));
        assert!(t.apply(Vec::new(), 2, 48_000, MS(1)).is_none(), "empty stays empty");
    }

    #[test]
    fn the_end_is_trimmed_inside_and_after_a_buffer() {
        let mut t = DelayTrim::new(Duration::ZERO).with_end(Some(MS(150)));
        assert_eq!(t.apply(buf(100, 1), 1, 1000, MS(0)).unwrap().frames(), 100);
        assert_eq!(t.apply(buf(100, 1), 1, 1000, MS(100)).unwrap().frames(), 50);
        assert!(t.apply(buf(100, 1), 1, 1000, MS(200)).is_none());
    }

    #[test]
    fn the_end_is_in_presentation_time() {
        // 10 ms of start-up padding: 100 decoded frames present as 90 frames from 0; the end at
        // 50 ms keeps 50 of them.
        let mut t = DelayTrim::new(MS(10)).with_end(Some(MS(50)));
        t.on_packet(MS(0));
        let b = t.apply(buf(100, 1), 1, 1000, MS(0)).unwrap();
        assert_eq!((b.frames(), b.pts), (50, MS(0)));
    }

    #[test]
    fn clip_end_without_an_end_keeps_everything() {
        assert_eq!(clip_end(buf(10, 2), 2, 1000, MS(5), None).len(), 20);
        assert_eq!(clip_end(buf(10, 2), 2, 1000, MS(5), Some(MS(8))).len(), 6);
    }
}
