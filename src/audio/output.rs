//! Audio sinks: a lock-free ring buffer drained by a device callback (cpal) or by a test.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

use super::Volume;
use crate::{Error, Result};

/// Ring buffer length, in time.
const BUFFER: Duration = Duration::from_millis(200);

/// Monotonic nanoseconds since the first call (a process-wide time base for anchors).
pub(crate) fn now_ns() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// What the device callback last reported, for the clock to interpolate from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Anchor {
    /// Seek epoch the callback rendered in; anchors from an older epoch are ignored.
    pub epoch: u64,
    /// Frames played in this epoch before this buffer.
    pub frames_before: u64,
    /// Frames in this buffer, heard progressively from `at_ns` on. 0 = count `frames_before`
    /// as already heard (no interpolation; used by `NullOutput`).
    pub len: u64,
    /// `now_ns()` when the buffer's first frame reaches the speaker.
    pub at_ns: u64,
}

/// State shared between the producer side (audio decode thread), the clock and the callback.
#[derive(Debug)]
pub(crate) struct OutputShared {
    pub rate: u32,
    pub channels: u16,
    pub volume: Arc<Volume>,
    /// Frames actually delivered to the device (silence from underruns is not counted).
    pub frames_played: AtomicU64,
    /// While paused the callback outputs silence and consumes nothing.
    pub paused: AtomicBool,
    /// Frames with a lower sequence number than this are discarded unplayed (seek flush).
    pub discard_until: AtomicU64,
    /// Seek epoch, bumped by the clock on every `set`.
    epoch: AtomicU64,
    /// Seqlock-protected latest `Anchor` (single writer: the callback).
    anchor_seq: AtomicU64,
    anchor: [AtomicU64; 4],
    /// The device reported a fatal error; audio is gone.
    pub failed: AtomicBool,
    /// Audio played everything it had (end of stream); time must go on without it.
    /// Cleared by a seek.
    pub exhausted: AtomicBool,
    /// Diagnostics: render calls, and the frames readable in the ring at the last one.
    pub renders: AtomicU64,
    pub ring_frames: AtomicU64,
}

impl OutputShared {
    fn new(rate: u32, channels: u16, volume: Arc<Volume>) -> Self {
        Self {
            rate,
            channels,
            volume,
            frames_played: AtomicU64::new(0),
            paused: AtomicBool::new(true),
            discard_until: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            anchor_seq: AtomicU64::new(0),
            anchor: Default::default(),
            failed: AtomicBool::new(false),
            exhausted: AtomicBool::new(false),
            renders: AtomicU64::new(0),
            ring_frames: AtomicU64::new(0),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Starts a new epoch (a seek); returns it.
    pub fn next_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Lock-free publish (seqlock writer). Only the callback calls this.
    pub fn publish(&self, a: Anchor) {
        let seq = self.anchor_seq.load(Ordering::Relaxed);
        self.anchor_seq.store(seq.wrapping_add(1), Ordering::Relaxed); // odd: writing
        fence(Ordering::Release);
        for (slot, v) in self.anchor.iter().zip([a.epoch, a.frames_before, a.len, a.at_ns]) {
            slot.store(v, Ordering::Relaxed);
        }
        self.anchor_seq.store(seq.wrapping_add(2), Ordering::Release); // even: stable
    }

    /// Consistent snapshot of the latest anchor (seqlock reader).
    pub fn anchor(&self) -> Anchor {
        loop {
            let s1 = self.anchor_seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let v: [u64; 4] = std::array::from_fn(|i| self.anchor[i].load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if self.anchor_seq.load(Ordering::Relaxed) == s1 {
                return Anchor { epoch: v[0], frames_before: v[1], len: v[2], at_ns: v[3] };
            }
        }
    }
}

/// The callback side: drains the ring buffer into device buffers.
pub(crate) struct Renderer {
    consumer: Consumer<f32>,
    shared: Arc<OutputShared>,
    /// Frames taken out of the ring so far (played or discarded).
    consumed: u64,
    /// Epoch of the frames below, and frames played in it so far.
    epoch: u64,
    epoch_frames: u64,
}

impl Renderer {
    /// Fills `out` and reports it as heard immediately (no interpolation; `NullOutput`).
    pub fn render(&mut self, out: &mut [f32]) {
        self.render_at(out, None);
    }

    /// Fills `out` (interleaved, `shared.channels` per frame) with the next samples. With
    /// `Some(delay)`, the buffer is announced as playing from `delay` from now on, so the clock
    /// can interpolate through it (device callbacks).
    pub fn render_at(&mut self, out: &mut [f32], delay: Option<Duration>) {
        let ch = self.shared.channels as usize;
        self.shared.renders.fetch_add(1, Ordering::Relaxed);
        self.shared.ring_frames.store((self.consumer.slots() / ch.max(1)) as u64, Ordering::Relaxed);
        let epoch = self.shared.epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.epoch_frames = 0;
        }
        // Drop samples that a seek made stale, even while paused.
        let discard = self.shared.discard_until.load(Ordering::Acquire);
        if self.consumed < discard {
            let frames = ((discard - self.consumed) as usize).min(self.consumer.slots() / ch);
            if let Ok(chunk) = self.consumer.read_chunk(frames * ch) {
                chunk.commit_all();
                self.consumed += frames as u64;
            }
        }
        if self.shared.paused.load(Ordering::Relaxed) || self.consumed < discard {
            out.fill(0.0);
            return;
        }
        let want = out.len() / ch;
        let frames = want.min(self.consumer.slots() / ch);
        let n = frames * ch;
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..n].copy_from_slice(b);
            chunk.commit_all();
        }
        let gain = self.shared.volume.effective();
        if gain != 1.0 {
            out[..n].iter_mut().for_each(|s| *s *= gain);
        }
        out[n..].fill(0.0);
        self.consumed += frames as u64;
        self.shared.frames_played.fetch_add(frames as u64, Ordering::Release);
        if frames > 0 {
            let before = self.epoch_frames;
            self.epoch_frames += frames as u64;
            self.shared.publish(match delay {
                Some(d) => Anchor { epoch, frames_before: before, len: frames as u64, at_ns: now_ns() + d.as_nanos() as u64 },
                None => Anchor { epoch, frames_before: self.epoch_frames, len: 0, at_ns: now_ns() },
            });
        }
    }
}

/// Renders into an f32 scratch buffer and converts to a device's integer sample format.
/// The scratch is sized once (half a second) so the real-time callback never allocates.
#[cfg(any(feature = "audio-output", test))]
pub(crate) struct Converter {
    scratch: Vec<f32>,
}

#[cfg(any(feature = "audio-output", test))]
impl Converter {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self { scratch: Vec::with_capacity((rate as usize / 2).max(1) * channels.max(1) as usize) }
    }

    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.scratch.capacity()
    }

    pub fn render_into<T>(&mut self, renderer: &mut Renderer, data: &mut [T], delay: Option<Duration>, convert: impl Fn(f32) -> T) {
        // Within capacity this only writes zeros; it allocates only for a callback longer than 0.5 s.
        self.scratch.resize(data.len(), 0.0);
        renderer.render_at(&mut self.scratch, delay);
        for (d, s) in data.iter_mut().zip(&self.scratch) {
            *d = convert(s.clamp(-1.0, 1.0));
        }
    }
}

/// What a player gets from opening an output.
pub(crate) struct OpenedOutput {
    pub shared: Arc<OutputShared>,
    pub producer: Producer<f32>,
    /// Keeps the device stream alive (dropped with the player).
    pub _guard: Option<Box<dyn Send + Sync>>,
}

fn ring(rate: u32, channels: u16, volume: Arc<Volume>) -> (Arc<OutputShared>, Producer<f32>, Renderer) {
    let shared = Arc::new(OutputShared::new(rate, channels, volume));
    let frames = (rate as u128 * BUFFER.as_millis() / 1000) as usize;
    let (producer, consumer) = RingBuffer::new(frames.max(1) * channels.max(1) as usize);
    let renderer = Renderer { consumer, shared: shared.clone(), consumed: 0, epoch: 0, epoch_frames: 0 };
    (shared, producer, renderer)
}

/// An output with no device: the owner pulls samples explicitly. Used by tests and for offline use.
pub struct NullOutput {
    rate: u32,
    channels: u16,
    renderer: Mutex<Option<Renderer>>,
}

impl NullOutput {
    /// Behaves as if the device disappeared (tests the player's fallback to wall-clock time).
    pub fn simulate_device_loss(&self) {
        if let Some(r) = self.renderer.lock().unwrap().as_ref() {
            r.shared.failed.store(true, Ordering::SeqCst);
        }
    }

    pub fn new(rate: u32, channels: u16) -> Arc<Self> {
        Arc::new(Self { rate, channels, renderer: Mutex::new(None) })
    }

    /// Renders like a real device: the buffer is heard progressively, starting `delay` from now.
    /// (`pull` instead counts frames as heard the moment they are pulled.)
    pub fn render_realtime(&self, frames: usize, delay: Duration) -> Vec<f32> {
        let mut out = vec![0.0; frames * self.channels as usize];
        if let Some(r) = self.renderer.lock().unwrap().as_mut() {
            r.render_at(&mut out, Some(delay));
        }
        out
    }

    /// Renders `frames` frames as a device callback would (silence until a player is attached).
    pub fn pull(&self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * self.channels as usize];
        if let Some(r) = self.renderer.lock().unwrap().as_mut() {
            r.render(&mut out);
        }
        out
    }

    pub(crate) fn attach(&self, volume: Arc<Volume>) -> OpenedOutput {
        let (shared, producer, renderer) = ring(self.rate, self.channels, volume);
        *self.renderer.lock().unwrap() = Some(renderer);
        OpenedOutput { shared, producer, _guard: None }
    }
}

/// Where audio goes.
#[derive(Clone, Default)]
pub enum AudioOutputConfig {
    /// The system's default output device (needs the `audio-output` feature).
    #[default]
    Default,
    /// A `NullOutput` the caller drives (tests, offline use).
    Null(Arc<NullOutput>),
    /// Ignore audio streams entirely (video plays on the system clock).
    Disabled,
}

impl std::fmt::Debug for AudioOutputConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Default => "Default",
            Self::Null(_) => "Null",
            Self::Disabled => "Disabled",
        })
    }
}

/// Opens the configured output. `Ok(None)` for `Disabled`.
pub(crate) fn open_output(config: &AudioOutputConfig, volume: Arc<Volume>) -> Result<Option<OpenedOutput>> {
    match config {
        AudioOutputConfig::Disabled => Ok(None),
        AudioOutputConfig::Null(null) => Ok(Some(null.attach(volume))),
        #[cfg(feature = "audio-output")]
        AudioOutputConfig::Default => cpal_output::open(volume).map(Some),
        #[cfg(not(feature = "audio-output"))]
        AudioOutputConfig::Default => Err(Error::Unsupported("audio output (enable the `audio-output` feature)")),
    }
}

#[cfg(feature = "audio-output")]
mod cpal_output {
    use std::sync::mpsc;
    use std::thread;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use super::*;

    struct Guard {
        stop: Option<mpsc::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.stop.take();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Errors after which the stream will not play again. Others (xruns, a changed default
    /// device while ours still works) are recoverable and must not cost us audio sync.
    pub(super) fn is_fatal(kind: cpal::ErrorKind) -> bool {
        use cpal::ErrorKind::*;
        matches!(kind, DeviceNotAvailable | StreamInvalidated | HostUnavailable)
    }

    /// Opens the default device on a dedicated thread (cpal streams are not `Send` everywhere).
    pub(super) fn open(volume: Arc<Volume>) -> Result<OpenedOutput> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(Arc<OutputShared>, Producer<f32>)>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = thread::Builder::new().name("audio-output".into()).spawn(move || {
            let stream = match build(volume) {
                Ok((stream, shared, producer)) => {
                    let _ = ready_tx.send(Ok((shared, producer)));
                    stream
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = stop_rx.recv(); // until the guard is dropped
            drop(stream);
        })?;
        let (shared, producer) = ready_rx
            .recv()
            .map_err(|_| Error::Unsupported("audio output thread exited"))??;
        Ok(OpenedOutput {
            shared,
            producer,
            _guard: Some(Box::new(Guard { stop: Some(stop_tx), thread: Some(thread) })),
        })
    }

    fn build(volume: Arc<Volume>) -> Result<(cpal::Stream, Arc<OutputShared>, Producer<f32>)> {
        let err = |what: &str, e: &dyn std::fmt::Display| Error::Decode(format!("audio output: {what}: {e}"));
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(Error::Unsupported("no audio output device"))?;
        let supported = device.default_output_config().map_err(|e| err("config", &e))?;
        let config = supported.config();
        let (shared, producer, mut renderer) = ring(config.sample_rate, config.channels, volume);
        let failed = shared.clone();
        let on_error = move |e: cpal::Error| {
            if is_fatal(e.kind()) {
                log::warn!("audio output lost: {e}");
                failed.failed.store(true, Ordering::Relaxed);
            } else {
                log::debug!("audio output hiccup: {e}");
            }
        };
        // How long until this callback's buffer reaches the speaker.
        let delay = |info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            Some(ts.playback.duration_since(ts.callback))
        };
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config,
                move |data: &mut [f32], info| renderer.render_at(data, delay(info)),
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => {
                let mut conv = Converter::new(config.sample_rate, config.channels);
                device.build_output_stream(
                    config,
                    move |data: &mut [i16], info| {
                        conv.render_into(&mut renderer, data, delay(info), |s| (s * i16::MAX as f32) as i16)
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut conv = Converter::new(config.sample_rate, config.channels);
                device.build_output_stream(
                    config,
                    move |data: &mut [u16], info| {
                        conv.render_into(&mut renderer, data, delay(info), |s| ((s + 1.0) * 0.5 * u16::MAX as f32) as u16)
                    },
                    on_error,
                    None,
                )
            }
            other => return Err(Error::Decode(format!("audio output: unsupported sample format {other:?}"))),
        }
        .map_err(|e| err("stream", &e))?;
        stream.play().map_err(|e| err("play", &e))?;
        Ok((stream, shared, producer))
    }
}

/// Producer side helper: pushes whole frames, waiting (with `keep_waiting` checks) when full.
/// While waiting it sleeps about as long as the device needs to free the space (2–20 ms), and
/// 20 ms while paused, instead of spinning. Returns the number of frames written (fewer than
/// given if `keep_waiting` said stop).
pub(crate) fn push_frames(
    producer: &mut Producer<f32>,
    out: &OutputShared,
    mut samples: &[f32],
    mut keep_waiting: impl FnMut() -> bool,
) -> usize {
    let ch = out.channels.max(1) as usize;
    let mut written = 0;
    while samples.len() >= ch {
        let n = (producer.slots() / ch * ch).min(samples.len() / ch * ch);
        if n == 0 {
            if !keep_waiting() {
                return written;
            }
            let wait = if out.paused.load(Ordering::Relaxed) {
                Duration::from_millis(20)
            } else {
                let needed = (samples.len() / ch).min(producer.buffer().capacity() / ch / 2) as u64;
                Duration::from_nanos(needed * 1_000_000_000 / out.rate.max(1) as u64)
                    .clamp(Duration::from_millis(2), Duration::from_millis(20))
            };
            std::thread::sleep(wait);
            continue;
        }
        if let Ok(mut chunk) = producer.write_chunk(n) {
            let (a, b) = chunk.as_mut_slices();
            a.copy_from_slice(&samples[..a.len()]);
            b.copy_from_slice(&samples[a.len()..n]);
            chunk.commit_all();
        }
        samples = &samples[n..];
        written += n / ch;
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attached() -> (Arc<NullOutput>, OpenedOutput, Arc<Volume>) {
        let null = NullOutput::new(1000, 2);
        let volume = Arc::new(Volume::default());
        let out = null.attach(volume.clone());
        (null, out, volume)
    }

    #[test]
    fn plays_pushed_frames_and_counts_them() {
        let (null, mut out, _) = attached();
        out.shared.paused.store(false, Ordering::Relaxed);
        assert_eq!(push_frames(&mut out.producer, &out.shared, &[0.1, 0.2, 0.3, 0.4], || true), 2);
        assert_eq!(null.pull(3), vec![0.1, 0.2, 0.3, 0.4, 0.0, 0.0], "underrun pads with silence");
        assert_eq!(out.shared.frames_played.load(Ordering::Relaxed), 2, "silence is not counted");
    }

    #[test]
    fn paused_output_consumes_nothing() {
        let (null, mut out, _) = attached();
        push_frames(&mut out.producer, &out.shared, &[0.5; 4], || true);
        assert_eq!(null.pull(2), vec![0.0; 4]);
        assert_eq!(out.shared.frames_played.load(Ordering::Relaxed), 0);
        out.shared.paused.store(false, Ordering::Relaxed);
        assert_eq!(null.pull(2), vec![0.5; 4]);
    }

    #[test]
    fn volume_and_mute_scale_samples() {
        let (null, mut out, volume) = attached();
        out.shared.paused.store(false, Ordering::Relaxed);
        push_frames(&mut out.producer, &out.shared, &[0.8; 8], || true);
        volume.set(0.5);
        assert_eq!(null.pull(2), vec![0.4; 4]);
        volume.set_muted(true);
        assert_eq!(null.pull(2), vec![0.0; 4]);
    }

    #[test]
    fn discard_drops_stale_frames_before_playing() {
        let (null, mut out, _) = attached();
        out.shared.paused.store(false, Ordering::Relaxed);
        push_frames(&mut out.producer, &out.shared, &[0.9; 6], || true); // frames 0..3 (stale)
        out.shared.discard_until.store(3, Ordering::Release);
        push_frames(&mut out.producer, &out.shared, &[0.1; 2], || true); // frame 3 (fresh)
        assert_eq!(null.pull(2), vec![0.1, 0.1, 0.0, 0.0]);
        assert_eq!(out.shared.frames_played.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn push_gives_up_when_asked_while_full() {
        let (_null, mut out, _) = attached();
        let capacity = out.producer.slots();
        let samples = vec![0.0; capacity + 2];
        let mut asked = 0;
        let written = push_frames(&mut out.producer, &out.shared, &samples, || {
            asked += 1;
            asked < 3
        });
        assert_eq!(written, capacity / 2, "fills what fits, then gives up");
    }

    #[cfg(feature = "audio-output")]
    #[test]
    fn only_device_loss_errors_are_fatal() {
        use cpal::ErrorKind::*;
        assert!(!cpal_output::is_fatal(Xrun), "an underrun is recoverable");
        assert!(cpal_output::is_fatal(DeviceNotAvailable));
        assert!(cpal_output::is_fatal(StreamInvalidated));
    }

    #[test]
    fn a_full_ring_while_paused_does_not_spin() {
        let (_null, mut out, _) = attached(); // paused, nobody consuming
        let fill = vec![0.0; out.producer.slots()];
        push_frames(&mut out.producer, &out.shared, &fill, || true);
        let start = std::time::Instant::now();
        let mut wakeups = 0;
        push_frames(&mut out.producer, &out.shared, &[0.0; 2], || {
            wakeups += 1;
            start.elapsed() < Duration::from_millis(200)
        });
        assert!(wakeups <= 15, "{wakeups} wake-ups in 200 ms while paused (was polling every 2 ms)");
    }

    #[test]
    fn integer_conversion_never_allocates_in_the_callback() {
        let (_null, mut out, _) = attached();
        out.shared.paused.store(false, Ordering::Relaxed);
        push_frames(&mut out.producer, &out.shared, &[0.5; 400], || true);
        let mut renderer =
            Renderer { consumer: RingBuffer::new(8).1, shared: out.shared.clone(), consumed: 0, epoch: 0, epoch_frames: 0 };
        // Sized for half a second at 1 kHz stereo, like the device setup does.
        let mut conv = Converter::new(1000, 2);
        let before = conv.capacity();
        let mut data = vec![0i16; 960];
        conv.render_into(&mut renderer, &mut data, None, |s| (s * i16::MAX as f32) as i16);
        assert_eq!(conv.capacity(), before, "the callback reallocated its scratch buffer");
    }
}
