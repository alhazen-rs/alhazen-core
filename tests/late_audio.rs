//! An audio decoder whose output appears some time after its input (like ffmpeg, which takes
//! ~100 ms to start) must still be heard when no further audio packets arrive for a while, e.g.
//! because video back-pressure has paused demuxing. Regression: the audio thread only collected
//! output right after a packet, so playback froze at 0 s with fast (GPU) video decoding.
#![cfg(feature = "native")]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alhazen_core::audio::{AudioOutputConfig, NullOutput};
use alhazen_core::backend::{Backend, Registry};
use alhazen_core::decode::{AudioBuffer, AudioDecoder, VideoDecoder};
use alhazen_core::demux::{Codec, ContainerFormat, Demuxer, Packet, StreamInfo};
use alhazen_core::source::MediaSource;
use alhazen_core::{FfmpegConfig, Player, PlayerConfig, Source};

/// Emits one buffer per packet, but only 100 ms after the packet arrived.
struct LateDecoder {
    rate: u32,
    channels: u16,
    pending: VecDeque<(Instant, AudioBuffer)>,
}

impl AudioDecoder for LateDecoder {
    fn send_packet(&mut self, p: &Packet) -> alhazen_core::Result<()> {
        let samples = vec![0.1; 1024 * self.channels as usize];
        let buf = AudioBuffer { rate: self.rate, channels: self.channels, samples, pts: p.pts };
        self.pending.push_back((Instant::now() + Duration::from_millis(100), buf));
        Ok(())
    }
    fn receive_samples(&mut self) -> alhazen_core::Result<Option<AudioBuffer>> {
        if self.pending.front().is_some_and(|(at, _)| *at <= Instant::now()) {
            return Ok(self.pending.pop_front().map(|(_, b)| b));
        }
        Ok(None)
    }
    fn flush(&mut self) {
        self.pending.clear();
    }
}

struct LateAudio;

impl Backend for LateAudio {
    fn name(&self) -> &'static str {
        "late-audio"
    }
    fn priority(&self) -> i32 {
        100
    }
    fn supports_container(&self, _: ContainerFormat) -> bool {
        false
    }
    fn open_demuxer(&self, _: ContainerFormat, _: Box<dyn MediaSource>) -> alhazen_core::Result<Box<dyn Demuxer>> {
        unreachable!()
    }
    fn supports_video(&self, _: &StreamInfo) -> bool {
        false
    }
    fn open_video_decoder(&self, _: &StreamInfo, _: usize) -> alhazen_core::Result<Box<dyn VideoDecoder>> {
        unreachable!()
    }
    fn supports_audio(&self, s: &StreamInfo) -> bool {
        s.codec == Codec::Aac
    }
    fn open_audio_decoder(&self, s: &StreamInfo) -> alhazen_core::Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(LateDecoder { rate: s.sample_rate, channels: s.channels, pending: VecDeque::new() }))
    }
}

#[test]
fn late_audio_output_is_collected_while_demuxing_waits() {
    let mut registry = Registry::empty_with_native();
    registry.register(Arc::new(LateAudio));
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig {
        audio_output: AudioOutputConfig::Null(null.clone()),
        registry: Some(Arc::new(registry)),
        ffmpeg: FfmpegConfig { enabled: false, ..Default::default() },
        // Tiny queues: video back-pressure stops demuxing long before the first audio output.
        packet_queue_len: 2,
        frame_queue_len: 2,
        ..Default::default()
    };
    let path = format!("{}/tests/fixtures/av1_aac.mp4", env!("CARGO_MANIFEST_DIR"));
    let player = Player::open(Source::parse(&path).unwrap(), config).unwrap();
    player.play();
    let start = Instant::now();
    let mut pulled = 0u64;
    while start.elapsed() < Duration::from_secs(3) && player.position() < Duration::from_millis(500) {
        let due = (start.elapsed().as_secs_f64() * 48_000.0) as u64;
        null.pull((due - pulled) as usize);
        pulled = due;
        let _ = player.current_frame(); // the renderer drawing, as any app does
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(player.position() >= Duration::from_millis(500), "stuck at {:?}", player.position());
}
