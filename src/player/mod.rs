//! The public `Player`: opens a source, runs the demux/decode threads, exposes frames.

mod audio_thread;
mod pipeline;
mod speed;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use crate::audio::{AudioClock, AudioOutputConfig, OutputShared, Volume, open_output};
use crate::backend::Registry;
use crate::clock::{Clock, SystemClock};
use crate::demux::{self, StreamInfo, StreamKind};
use crate::frame::{FrameQueue, VideoFrame};
use crate::source::Source;
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub enum PlayerState {
    Loading,
    /// Playback requested, waiting for the first frame (after open or seek).
    Buffering,
    Playing,
    Paused,
    Ended,
    Error(Arc<Error>),
}

impl PlayerState {
    pub fn is_error(&self) -> bool {
        matches!(self, PlayerState::Error(_))
    }
}

impl PartialEq for PlayerState {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    StateChanged(PlayerState),
    /// The first frame after open or seek is ready (useful to repaint while paused).
    FrameReady,
    /// Non-fatal problem, e.g. an unsupported audio track.
    Warning(String),
    Error(Arc<Error>),
    Ended,
}

/// Diagnostics for a playing `Player`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerStats {
    /// The backend decoding video (`"native"`, `"ffmpeg-cli"`, …), if there is video.
    pub video_backend: Option<&'static str>,
    /// Frames decoded but never shown because they were late: discarded by the renderer, or
    /// skipped by the decode thread while catching up.
    pub frames_dropped: u64,
}

#[derive(Clone)]
pub struct PlayerConfig {
    /// Threads used inside the video decoder.
    pub decoder_threads: usize,
    /// Pool for color conversion. `None` uses one process-wide pool shared by all players.
    pub thread_pool: Option<Arc<rayon::ThreadPool>>,
    pub frame_queue_len: usize,
    pub packet_queue_len: usize,
    /// Backend names to try first, e.g. `vec!["ffmpeg-link", "native"]`.
    pub backend_order: Option<Vec<&'static str>>,
    pub autoplay: bool,
    /// `None` uses `Registry::with_defaults()`.
    pub registry: Option<Arc<Registry>>,
    /// Clock used when audio is not the master (no audio, or audio disabled/unavailable).
    /// `None` uses a `SystemClock`. Tests inject a `MockClock`.
    pub clock: Option<Arc<dyn Clock>>,
    /// Where audio goes. With an output, audio is the master clock and `clock` is not used.
    pub audio_output: AudioOutputConfig,
    /// How the `ffmpeg-cli` backend finds ffmpeg (ignored when `registry` is set).
    pub ffmpeg: crate::FfmpegConfig,
    /// When video decoding cannot keep up with playback, switch (once) to the next backend that
    /// supports the stream, in practice `ffmpeg-cli` with hardware decoding. (A decoder that
    /// can't decode the stream at all is always replaced, whatever this says.)
    pub auto_fallback: bool,
    /// Largest frame size wanted, in pixels (usually the display area in device pixels). Larger
    /// frames are scaled down before colour conversion. `None`: full size. Change it while
    /// playing with `Player::set_max_output_size`.
    pub max_output_size: Option<(u32, u32)>,
    /// Prefer the platform's (GPU) decoders over the native ones for codecs both handle (VP9 and
    /// AV1 through Media Foundation on Windows). `false` keeps native first. Ignored when
    /// `registry` is set.
    pub prefer_hardware: bool,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self {
            decoder_threads: cpus.min(8),
            thread_pool: None,
            frame_queue_len: 4,
            packet_queue_len: 64,
            backend_order: None,
            autoplay: false,
            registry: None,
            clock: None,
            audio_output: AudioOutputConfig::Default,
            ffmpeg: crate::FfmpegConfig::default(),
            auto_fallback: true,
            max_output_size: None,
            prefer_hardware: true,
        }
    }
}

/// How to get a decoder for a codec the build cannot play, for warnings.
fn codec_hint(codec: &demux::Codec) -> &'static str {
    match codec {
        demux::Codec::Aac => " (install ffmpeg, or enable alhazen-core's `native-aac` feature)",
        demux::Codec::H264 | demux::Codec::Hevc => " (install ffmpeg)",
        _ => "",
    }
}

/// The process-wide conversion pool shared by every player that does not supply its own.
pub fn shared_thread_pool() -> Arc<rayon::ThreadPool> {
    static POOL: OnceLock<Arc<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("video-convert-{i}"))
                .build()
                .expect("failed to build video conversion thread pool"),
        )
    })
    .clone()
}

/// How long `Drop` waits for pipeline threads before detaching ones blocked in I/O.
const DROP_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) enum Command {
    Seek { target: Duration, generation: u64 },
}

/// State shared between the `Player` handle and its threads.
pub(crate) struct Shared {
    pub state: Mutex<PlayerState>,
    pub clock: Arc<dyn Clock>,
    pub queue: FrameQueue,
    pub generation: AtomicU64,
    /// Generation whose first frame has been queued.
    pub ready_generation: AtomicU64,
    pub wants_play: AtomicBool,
    pub shutdown: AtomicBool,
    pub last_frame: Mutex<Option<VideoFrame>>,
    pub events: Sender<PlayerEvent>,
    pub duration: Option<Duration>,
    /// Serializes every playback-control decision: seeks (generation + clock), the decode
    /// threads' generation checks and first-frame start, play/pause, and Ended. Lock order:
    /// `seek_lock` before `state`.
    pub seek_lock: Mutex<()>,
    pub has_video: bool,
    /// Audio is being played (false if absent, disabled, or given up after errors).
    pub audio_active: AtomicBool,
    /// The audio output drives `clock` (an `AudioClock`).
    pub audio_master: bool,
    pub audio_out: Option<Arc<OutputShared>>,
    pub volume: Arc<Volume>,
    /// Generation in which each stream reached its end and drained (u64::MAX = not yet).
    pub video_done: AtomicU64,
    pub audio_done: AtomicU64,
    /// Generation for which `Ended` was announced (so it is announced once).
    pub ended_generation: AtomicU64,
    /// To the demux thread (seeks).
    pub commands: Sender<Command>,
    pub seekable: bool,
    /// Name of the backend decoding video (changes on a speed fallback).
    pub video_backend: Mutex<Option<&'static str>>,
    /// `max_output_size` packed as `w << 32 | h`; 0 = no limit.
    pub max_output_size: AtomicU64,
}

/// `PlayerConfig::max_output_size` in its atomic form.
pub(crate) fn pack_size(size: Option<(u32, u32)>) -> u64 {
    size.map_or(0, |(w, h)| (w as u64) << 32 | h as u64)
}

impl Shared {
    /// The current output size limit.
    pub fn max_output_size(&self) -> Option<(u32, u32)> {
        let v = self.max_output_size.load(Ordering::Relaxed);
        (v != 0).then_some(((v >> 32) as u32, v as u32))
    }
}

impl Shared {
    pub fn state(&self) -> PlayerState {
        self.state.lock().unwrap().clone()
    }

    pub fn set_state(&self, new: PlayerState) {
        let mut s = self.state.lock().unwrap();
        if *s == new && !new.is_error() {
            return;
        }
        // Errors are terminal.
        if s.is_error() {
            return;
        }
        *s = new.clone();
        drop(s);
        let _ = self.events.send(PlayerEvent::StateChanged(new));
    }

    /// Records that a stream drained to its end in `generation`, and announces `Ended` once every
    /// active stream has.
    pub fn stream_finished(&self, generation: u64, video: bool) {
        let done = if video { &self.video_done } else { &self.audio_done };
        done.store(generation, Ordering::SeqCst);
        self.check_ended(generation);
    }

    pub fn check_ended(&self, generation: u64) {
        let _guard = self.seek_lock.lock().unwrap();
        if generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        let video_ok = !self.has_video || self.video_done.load(Ordering::SeqCst) == generation;
        let audio_ok = !self.audio_active.load(Ordering::SeqCst) || self.audio_done.load(Ordering::SeqCst) == generation;
        if !(video_ok && audio_ok) || self.ended_generation.swap(generation, Ordering::SeqCst) == generation {
            return;
        }
        self.clock.pause();
        self.wants_play.store(false, Ordering::SeqCst);
        self.set_state(PlayerState::Ended);
        let _ = self.events.send(PlayerEvent::Ended);
    }

    /// Playback was requested: start now if the current seek's first frame is ready, else buffer.
    /// `request_play`, `request_pause` and `frame_ready` decide under `seek_lock`, so the UI and
    /// the decode threads can never interleave into "Paused with a running clock" or
    /// "Buffering although ready".
    pub fn request_play(&self) {
        let _guard = self.seek_lock.lock().unwrap();
        self.wants_play.store(true, Ordering::SeqCst);
        if self.ready_generation.load(Ordering::SeqCst) == self.generation.load(Ordering::SeqCst) {
            self.clock.resume();
            self.set_state(PlayerState::Playing);
        } else {
            self.set_state(PlayerState::Buffering);
        }
    }

    pub fn request_pause(&self) {
        let _guard = self.seek_lock.lock().unwrap();
        self.wants_play.store(false, Ordering::SeqCst);
        self.clock.pause();
        if self.state() != PlayerState::Ended {
            self.set_state(PlayerState::Paused);
        }
    }

    /// The first frame (video) or audio of `generation` is queued: start if playback was requested.
    /// Returns `true` the first time for that generation.
    pub fn frame_ready(&self, generation: u64) -> bool {
        let _guard = self.seek_lock.lock().unwrap();
        if self.ready_generation.swap(generation, Ordering::SeqCst) == generation {
            return false;
        }
        if self.wants_play.load(Ordering::SeqCst) {
            self.clock.resume();
            if self.state() == PlayerState::Buffering {
                self.set_state(PlayerState::Playing);
            }
        }
        true
    }

    /// Stops audio for the rest of playback (errors, device loss); video carries on.
    pub fn disable_audio(&self, why: &str) {
        if self.audio_active.swap(false, Ordering::SeqCst) {
            if let Some(out) = &self.audio_out {
                // Hands the clock over to wall time from the current position.
                out.failed.store(true, Ordering::SeqCst);
            }
            let _ = self.events.send(PlayerEvent::Warning(format!("audio disabled: {why}")));
            self.check_ended(self.generation.load(Ordering::SeqCst));
        }
    }

    /// Frame-accurate seek; see `Player::seek`.
    pub fn seek(&self, to: Duration) {
        self.seek_where(|_| Some(to));
    }

    /// Seeks to the current playback position, unless a seek has happened since `generation`
    /// (the decoder fallback's resync must never undo a newer user seek). Returns whether it did.
    pub fn seek_to_now_if_current(&self, generation: u64) -> bool {
        self.seek_where(|s| (s.generation.load(Ordering::SeqCst) == generation).then(|| s.clock.now()))
    }

    /// Seeks to `target(self)`, decided under `seek_lock`; `None` means no seek.
    fn seek_where(&self, target: impl FnOnce(&Self) -> Option<Duration>) -> bool {
        if self.state().is_error() || !self.seekable {
            return false;
        }
        let guard = self.seek_lock.lock().unwrap();
        let Some(to) = target(self) else { return false };
        let to = self.duration.map_or(to, |d| to.min(d));
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.queue.clear(generation);
        if let Some(out) = &self.audio_out {
            // Silence everything already queued; the audio thread re-opens playback for the
            // samples it decodes after this seek.
            out.discard_until.store(u64::MAX, Ordering::SeqCst);
            // Audio will play again from the target, so it drives the clock again.
            out.exhausted.store(false, Ordering::SeqCst);
        }
        self.clock.pause();
        self.clock.set(to);
        drop(guard);
        if self.wants_play.load(Ordering::SeqCst) {
            self.set_state(PlayerState::Buffering);
        } else {
            self.set_state(PlayerState::Paused);
        }
        let _ = self.commands.send(Command::Seek { target: to, generation });
        true
    }

    pub fn fail(&self, err: Error) {
        let err = Arc::new(err);
        self.clock.pause();
        self.set_state(PlayerState::Error(err.clone()));
        let _ = self.events.send(PlayerEvent::Error(err));
    }
}

pub struct Player {
    shared: Arc<Shared>,
    events: Receiver<PlayerEvent>,
    threads: Vec<JoinHandle<()>>,
    video_size: (u32, u32),
    seekable: bool,
    /// Keeps the audio device stream alive; dropped after the threads.
    _audio_guard: Option<Box<dyn Send + Sync>>,
}

impl Player {
    /// Opens the source and probes/creates the demuxer and decoder on the calling thread
    /// (call it off the UI thread), then starts the pipeline threads.
    pub fn open(source: Source, config: PlayerConfig) -> Result<Player> {
        let registry = config.registry.clone().unwrap_or_else(|| Arc::new(Registry::with_options(&config.ffmpeg, config.prefer_hardware)));
        let order = config.backend_order.as_deref();

        let mut src = source.open()?;
        let seekable = src.is_seekable() && !src.is_live();
        let format = demux::probe(src.as_mut())?.ok_or(Error::UnsupportedContainer)?;
        let demuxer = registry.open_demuxer(&source, format, src, order)?;
        let streams = demuxer.streams().to_vec();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let warn = |msg: String| {
            let _ = event_tx.send(PlayerEvent::Warning(msg));
        };

        let video = streams.iter().find(|s| s.kind == StreamKind::Video).cloned();
        let video_decoder = match &video {
            Some(v) => Some(registry.open_video_decoder_except(v, config.decoder_threads, order, None)?),
            None => None,
        };

        // Audio: the default-flagged track, else the first one.
        let audio_streams: Vec<&StreamInfo> = streams.iter().filter(|s| s.kind == StreamKind::Audio).collect();
        let audio_stream = audio_streams.iter().find(|s| s.default).or(audio_streams.first()).map(|s| (*s).clone());
        let volume = Arc::new(Volume::default());
        let mut audio = None;
        if let Some(a) = &audio_stream
            && !matches!(config.audio_output, AudioOutputConfig::Disabled)
        {
            match registry.open_audio_decoder(a, order) {
                Ok(decoder) => match open_output(&config.audio_output, volume.clone()) {
                    Ok(Some(out)) => audio = Some((a.clone(), decoder, out)),
                    Ok(None) => {}
                    Err(e) => warn(format!("no audio output ({e}); playing without sound")),
                },
                Err(e) if video.is_some() => warn(format!("audio track {} not played: {e}{}", a.id, codec_hint(&a.codec))),
                Err(e) => return Err(e),
            }
        }
        if video.is_none() && audio.is_none() {
            return Err(match audio_stream {
                Some(_) => Error::Unsupported("audio-only media with audio disabled or no output"),
                None => Error::Unsupported("media without a playable video or audio stream"),
            });
        }

        let audio_out = audio.as_ref().map(|(_, _, out)| out.shared.clone());
        let clock: Arc<dyn Clock> = match &audio_out {
            Some(out) => Arc::new(AudioClock::new(out.clone())),
            None => config.clock.clone().unwrap_or_else(|| Arc::new(SystemClock::new())),
        };
        clock.pause();
        clock.set(Duration::ZERO);
        let duration = video.as_ref().and_then(|v| v.duration).or(audio_stream.as_ref().and_then(|a| a.duration));
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            state: Mutex::new(PlayerState::Paused),
            clock,
            queue: FrameQueue::new(config.frame_queue_len),
            generation: AtomicU64::new(0),
            ready_generation: AtomicU64::new(u64::MAX),
            wants_play: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            last_frame: Mutex::new(None),
            events: event_tx.clone(),
            duration,
            seek_lock: Mutex::new(()),
            has_video: video.is_some(),
            audio_active: AtomicBool::new(audio.is_some()),
            audio_master: audio_out.is_some(),
            audio_out,
            volume,
            video_done: AtomicU64::new(u64::MAX),
            audio_done: AtomicU64::new(u64::MAX),
            ended_generation: AtomicU64::new(u64::MAX),
            commands: cmd_tx,
            seekable,
            video_backend: Mutex::new(video_decoder.as_ref().map(|(name, _)| *name)),
            max_output_size: AtomicU64::new(pack_size(config.max_output_size)),
        });
        let pool = config.thread_pool.clone().unwrap_or_else(shared_thread_pool);
        let mut audio_guard = None;
        let audio_pipe = audio.map(|(info, decoder, out)| {
            audio_guard = out._guard;
            pipeline::AudioPipe { info, decoder, producer: out.producer, out: out.shared }
        });
        let threads = pipeline::spawn(
            shared.clone(),
            demuxer,
            video.as_ref().zip(video_decoder).map(|(v, (backend, decoder))| pipeline::VideoPipe {
                stream: v.id,
                decoder,
                fallback: Some(pipeline::Fallback {
                    registry: registry.clone(),
                    stream: v.clone(),
                    threads: config.decoder_threads,
                    order: config.backend_order.clone(),
                    current: backend,
                    speed: config.auto_fallback,
                }),
            }),
            audio_pipe,
            cmd_rx,
            config.packet_queue_len,
            pool,
        )?;

        let player = Player {
            shared,
            events: event_rx,
            threads,
            video_size: video.as_ref().map(|v| (v.width, v.height)).unwrap_or_default(),
            seekable,
            _audio_guard: audio_guard,
        };
        if config.autoplay {
            player.play();
        }
        Ok(player)
    }

    pub fn play(&self) {
        let s = &self.shared;
        match s.state() {
            PlayerState::Error(_) | PlayerState::Playing | PlayerState::Buffering => return,
            PlayerState::Ended if !self.seekable => {
                let _ = s.events.send(PlayerEvent::Warning("cannot restart: source is not seekable".into()));
                return;
            }
            PlayerState::Ended => self.seek(Duration::ZERO),
            _ => {}
        }
        s.request_play();
    }

    pub fn pause(&self) {
        let s = &self.shared;
        if s.state().is_error() {
            return;
        }
        s.request_pause();
    }

    /// Frame-accurate seek. Takes effect immediately for `position()`; frames follow shortly.
    pub fn seek(&self, to: Duration) {
        let s = &self.shared;
        if s.state().is_error() {
            return;
        }
        if !self.seekable {
            let _ = s.events.send(PlayerEvent::Warning("seek ignored: source is not seekable".into()));
            return;
        }
        s.seek(to);
    }

    pub fn state(&self) -> PlayerState {
        self.shared.state()
    }

    /// Largest frame size wanted from now on (e.g. the display area in device pixels, updated
    /// when the window is resized); larger frames are scaled down. `None`: full size.
    pub fn set_max_output_size(&self, size: Option<(u32, u32)>) {
        self.shared.max_output_size.store(pack_size(size), Ordering::Relaxed);
    }

    pub fn stats(&self) -> PlayerStats {
        PlayerStats {
            video_backend: *self.shared.video_backend.lock().unwrap(),
            frames_dropped: self.shared.queue.dropped() + self.shared.queue.skipped(),
        }
    }

    pub fn position(&self) -> Duration {
        let now = self.shared.clock.now();
        self.shared.duration.map_or(now, |d| now.min(d))
    }

    /// `false` for streams that cannot seek (e.g. HTTP servers without Range support);
    /// `seek` is then ignored with a `Warning` event.
    pub fn is_seekable(&self) -> bool {
        self.seekable
    }

    pub fn duration(&self) -> Option<Duration> {
        self.shared.duration
    }

    pub fn has_video(&self) -> bool {
        self.shared.has_video
    }

    /// Whether sound is currently being played.
    pub fn has_audio(&self) -> bool {
        self.shared.audio_active.load(Ordering::SeqCst)
    }

    /// 0.0..=1.0 (clamped). Applied instantly.
    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.set(volume);
    }

    pub fn volume(&self) -> f32 {
        self.shared.volume.get()
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.volume.set_muted(muted);
    }

    pub fn is_muted(&self) -> bool {
        self.shared.volume.is_muted()
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        (self.video_size.0 > 0).then_some(self.video_size)
    }

    /// The frame to show now. Cheap and non-blocking; call once per UI frame.
    /// Returns the previous frame again if no newer one is due yet.
    pub fn current_frame(&self) -> Option<VideoFrame> {
        let mut last = self.shared.last_frame.lock().unwrap();
        if let Some(f) = self.shared.queue.frame_for(self.shared.clock.now()) {
            *last = Some(f);
        }
        last.clone()
    }

    /// Event stream. Intended for a single consumer.
    pub fn events(&self) -> Receiver<PlayerEvent> {
        self.events.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.shared.queue.close();
        // Threads poll the shutdown flag every few ms, except while blocked in I/O (e.g. a
        // stalled HTTP read). Never let that block the caller, which is usually the UI thread:
        // detach such a thread; it holds only shared state and exits once its read returns.
        let deadline = Instant::now() + DROP_JOIN_TIMEOUT;
        while self.threads.iter().any(|t| !t.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        for t in self.threads.drain(..) {
            if t.is_finished() {
                let _ = t.join();
            } else {
                log::warn!("detaching {:?}: still blocked in I/O at drop", t.thread().name());
            }
        }
    }
}
