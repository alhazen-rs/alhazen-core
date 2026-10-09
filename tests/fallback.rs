//! Speed-based fallback: a decoder that cannot keep up is replaced, once, by the next backend.
#![cfg(feature = "native")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use alhazen_core::audio::{AudioOutputConfig, NullOutput};
use alhazen_core::backend::{Backend, NativeBackend, Registry};
use alhazen_core::clock::MockClock;
use alhazen_core::decode::{Av1Decoder, DecodedFrame, VideoDecoder};
use alhazen_core::demux::{Codec, ContainerFormat, Demuxer, Packet, StreamInfo};
use alhazen_core::source::MediaSource;
use alhazen_core::{Player, PlayerConfig, PlayerEvent, PlayerState, Result, Source};

/// AV1 through rav1d, made `delay` slower per frame, plus a one-off `stall` at frame
/// `STALL_FRAME`, 1.5 s in: well into playback, past anything decoded ahead before it starts.
struct Slowed {
    inner: Av1Decoder,
    delay: Duration,
    stall: Option<Duration>,
    frames: u32,
}

impl VideoDecoder for Slowed {
    fn send_packet(&mut self, p: &Packet) -> Result<()> {
        self.inner.send_packet(p)
    }
    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        let f = self.inner.receive_frame()?;
        if f.is_some() {
            self.frames += 1;
            let stall = if self.frames == STALL_FRAME { self.stall.take().unwrap_or_default() } else { Duration::ZERO };
            std::thread::sleep(self.delay + stall);
        }
        Ok(f)
    }
    fn flush(&mut self) {
        self.inner.flush()
    }
}

const STALL_FRAME: u32 = 45;

/// A video-only AV1 backend; counts how many decoders it opened.
struct Av1Backend {
    name: &'static str,
    priority: i32,
    delay: Duration,
    opened: Arc<AtomicUsize>,
    /// One-off stall before the second frame.
    lag: Option<Duration>,
}

impl Backend for Av1Backend {
    fn name(&self) -> &'static str {
        self.name
    }
    fn priority(&self) -> i32 {
        self.priority
    }
    fn supports_container(&self, _: ContainerFormat) -> bool {
        false
    }
    fn open_demuxer(&self, _: ContainerFormat, _: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>> {
        unreachable!()
    }
    fn supports_video(&self, s: &StreamInfo) -> bool {
        s.codec == Codec::Av1
    }
    fn open_video_decoder(&self, _: &StreamInfo, threads: usize) -> Result<Box<dyn VideoDecoder>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Slowed { inner: Av1Decoder::new(threads)?, delay: self.delay, stall: self.lag, frames: 0 }))
    }
}

fn registry(slow: &Arc<AtomicUsize>, fast: &Arc<AtomicUsize>, lag: Option<Duration>) -> Arc<Registry> {
    let mut r = Registry::empty();
    // Native only demuxes here: the two AV1 backends outrank it.
    r.register(Arc::new(NativeBackend));
    r.register(Arc::new(Av1Backend { name: "slow", priority: 20, delay: if lag.is_some() { Duration::from_millis(25) } else { Duration::from_millis(50) }, opened: slow.clone(), lag }));
    r.register(Arc::new(Av1Backend { name: "fast", priority: 10, delay: Duration::ZERO, opened: fast.clone(), lag: None }));
    Arc::new(r)
}

fn open(auto_fallback: bool) -> (Player, Arc<MockClock>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    open_with(auto_fallback, None)
}

fn open_with(auto_fallback: bool, lag: Option<Duration>) -> (Player, Arc<MockClock>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (slow, fast) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let clock = Arc::new(MockClock::new());
    let config = PlayerConfig {
        decoder_threads: 1,
        registry: Some(registry(&slow, &fast, lag)),
        clock: Some(clock.clone()),
        audio_output: AudioOutputConfig::Disabled,
        auto_fallback,
        ..Default::default()
    };
    let src = Source::parse(&format!("{}/tests/fixtures/av1.webm", env!("CARGO_MANIFEST_DIR"))).unwrap();
    (Player::open(src, config).unwrap(), clock, slow, fast)
}

/// Plays in real time-ish: the clock advances 33 ms per 33 ms of wall time.
fn play_until_end(player: &Player, clock: &MockClock, mut each: impl FnMut(Duration)) {
    player.play();
    let start = Instant::now();
    while player.state() != PlayerState::Ended {
        assert!(start.elapsed() < Duration::from_secs(20), "never ended");
        if let Some(f) = player.current_frame() {
            each(f.pts());
        }
        if player.state() == PlayerState::Playing {
            clock.advance(Duration::from_millis(33));
        }
        std::thread::sleep(Duration::from_millis(33));
    }
}

#[test]
fn too_slow_decoder_is_replaced_once_and_playback_continues() {
    let (player, clock, slow, fast) = open(true);
    let events = player.events();
    assert_eq!(player.stats().video_backend, Some("slow"));
    let mut shown = vec![];
    play_until_end(&player, &clock, |pts| {
        if shown.last() != Some(&pts) {
            shown.push(pts)
        }
    });
    assert_eq!((slow.load(Ordering::SeqCst), fast.load(Ordering::SeqCst)), (1, 1), "exactly one switch");
    assert_eq!(player.stats().video_backend, Some("fast"), "stats report the backend in use");
    let warnings: Vec<String> =
        events.try_iter().filter_map(|e| if let PlayerEvent::Warning(w) = e { Some(w) } else { None }).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("switching to fast"), "{warnings:?}");
    // Continuous: never jumps back to the start, and reaches the end.
    assert!(shown.windows(2).all(|w| w[1] >= w[0]), "timestamps went backwards: {shown:?}");
    assert!(*shown.last().unwrap() >= Duration::from_millis(1900), "{shown:?}");
}

#[test]
fn auto_fallback_off_keeps_the_slow_decoder() {
    let (player, clock, slow, fast) = open(false);
    play_until_end(&player, &clock, |_| {});
    assert_eq!((slow.load(Ordering::SeqCst), fast.load(Ordering::SeqCst)), (1, 0));
}

/// A decoder fast enough on average (25 ms per 33 ms frame) that fell behind once (an 800 ms
/// stall 1.5 s into playback) while the sound plays on: catching up only 8 ms per frame, the
/// picture stays more than 100 ms behind the audio clock for over 1.5 s, so the decoder is
/// replaced. (With an audio clock the video can't make the clock wait, as it does in video-only
/// playback, where such a stall costs a few dropped frames and needs no switch.)
#[test]
fn decoder_that_fell_behind_the_sound_is_replaced() {
    let (slow, fast) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let null = NullOutput::new(48_000, 2);
    let config = PlayerConfig {
        decoder_threads: 1,
        registry: Some(registry(&slow, &fast, Some(Duration::from_millis(800)))),
        audio_output: AudioOutputConfig::Null(null.clone()),
        auto_fallback: true,
        ..Default::default()
    };
    let src = Source::parse(&format!("{}/tests/fixtures/av1_8s_with_audio.webm", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let player = Player::open(src, config).unwrap();
    let events = player.events();
    player.play();
    let start = Instant::now();
    let mut loops = 0u64;
    while player.state() != PlayerState::Ended {
        loops += 1;
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "never ended after {loops} loops: {:?} at {:?}, slow {} fast {}, {:?}\n{}",
            player.state(),
            player.position(),
            slow.load(Ordering::SeqCst),
            fast.load(Ordering::SeqCst),
            player.stats(),
            player.debug_snapshot()
        );
        player.current_frame(); // a UI drawing at 100 Hz
        null.pull(48_000 / 100); // the sound plays in real time: 10 ms every 10 ms
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!((slow.load(Ordering::SeqCst), fast.load(Ordering::SeqCst)), (1, 1));
    assert!(events.try_iter().any(|e| matches!(e, PlayerEvent::Warning(w) if w.contains("switching to fast"))));
}
