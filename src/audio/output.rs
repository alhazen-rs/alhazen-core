//! Audio sinks: a lock-free ring buffer drained by a device callback (cpal) or by a test.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

use super::Volume;
use crate::{Error, Result};

/// Ring buffer length, in time.
const BUFFER: Duration = Duration::from_millis(200);

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
    /// Device output latency estimate.
    pub latency_ns: AtomicU64,
    /// The device reported an error; audio is gone.
    pub failed: AtomicBool,
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
            latency_ns: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
    }

    pub fn latency(&self) -> Duration {
        Duration::from_nanos(self.latency_ns.load(Ordering::Relaxed))
    }
}

/// The callback side: drains the ring buffer into device buffers.
pub(crate) struct Renderer {
    consumer: Consumer<f32>,
    shared: Arc<OutputShared>,
    /// Frames taken out of the ring so far (played or discarded).
    consumed: u64,
}

impl Renderer {
    /// Fills `out` (interleaved, `shared.channels` per frame) with the next samples.
    pub fn render(&mut self, out: &mut [f32]) {
        let ch = self.shared.channels as usize;
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
    let renderer = Renderer { consumer, shared: shared.clone(), consumed: 0 };
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
        let on_error = move |e| {
            log::warn!("audio output error: {e}");
            failed.failed.store(true, Ordering::Relaxed);
        };
        let latency = shared.clone();
        let note_latency = move |info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let d = ts.playback.duration_since(ts.callback);
            latency.latency_ns.store(d.as_nanos() as u64, Ordering::Relaxed);
        };
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config,
                move |data: &mut [f32], info| {
                    note_latency(info);
                    renderer.render(data);
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => {
                let mut scratch = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [i16], info| {
                        note_latency(info);
                        scratch.resize(data.len(), 0.0);
                        renderer.render(&mut scratch);
                        for (d, s) in data.iter_mut().zip(&scratch) {
                            *d = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut scratch = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [u16], info| {
                        note_latency(info);
                        scratch.resize(data.len(), 0.0);
                        renderer.render(&mut scratch);
                        for (d, s) in data.iter_mut().zip(&scratch) {
                            *d = ((s.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16;
                        }
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
/// Returns the number of frames written (fewer than given if `keep_waiting` said stop).
pub(crate) fn push_frames(
    producer: &mut Producer<f32>,
    channels: u16,
    mut samples: &[f32],
    mut keep_waiting: impl FnMut() -> bool,
) -> usize {
    let ch = channels.max(1) as usize;
    let mut written = 0;
    while samples.len() >= ch {
        let n = (producer.slots() / ch * ch).min(samples.len() / ch * ch);
        if n == 0 {
            if !keep_waiting() {
                return written;
            }
            std::thread::sleep(Duration::from_millis(2));
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
        assert_eq!(push_frames(&mut out.producer, 2, &[0.1, 0.2, 0.3, 0.4], || true), 2);
        assert_eq!(null.pull(3), vec![0.1, 0.2, 0.3, 0.4, 0.0, 0.0], "underrun pads with silence");
        assert_eq!(out.shared.frames_played.load(Ordering::Relaxed), 2, "silence is not counted");
    }

    #[test]
    fn paused_output_consumes_nothing() {
        let (null, mut out, _) = attached();
        push_frames(&mut out.producer, 2, &[0.5; 4], || true);
        assert_eq!(null.pull(2), vec![0.0; 4]);
        assert_eq!(out.shared.frames_played.load(Ordering::Relaxed), 0);
        out.shared.paused.store(false, Ordering::Relaxed);
        assert_eq!(null.pull(2), vec![0.5; 4]);
    }

    #[test]
    fn volume_and_mute_scale_samples() {
        let (null, mut out, volume) = attached();
        out.shared.paused.store(false, Ordering::Relaxed);
        push_frames(&mut out.producer, 2, &[0.8; 8], || true);
        volume.set(0.5);
        assert_eq!(null.pull(2), vec![0.4; 4]);
        volume.set_muted(true);
        assert_eq!(null.pull(2), vec![0.0; 4]);
    }

    #[test]
    fn discard_drops_stale_frames_before_playing() {
        let (null, mut out, _) = attached();
        out.shared.paused.store(false, Ordering::Relaxed);
        push_frames(&mut out.producer, 2, &[0.9; 6], || true); // frames 0..3 (stale)
        out.shared.discard_until.store(3, Ordering::Release);
        push_frames(&mut out.producer, 2, &[0.1; 2], || true); // frame 3 (fresh)
        assert_eq!(null.pull(2), vec![0.1, 0.1, 0.0, 0.0]);
        assert_eq!(out.shared.frames_played.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn push_gives_up_when_asked_while_full() {
        let (_null, mut out, _) = attached();
        let capacity = out.producer.slots();
        let samples = vec![0.0; capacity + 2];
        let mut asked = 0;
        let written = push_frames(&mut out.producer, 2, &samples, || {
            asked += 1;
            asked < 3
        });
        assert_eq!(written, capacity / 2, "fills what fits, then gives up");
    }
}
