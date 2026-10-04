//! The public `Player`: opens a source, runs the demux/decode threads, exposes frames.

mod pipeline;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use crate::backend::Registry;
use crate::clock::{Clock, SystemClock};
use crate::demux::{self, StreamKind};
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
    /// `None` uses a `SystemClock`. Tests inject a `MockClock`.
    pub clock: Option<Arc<dyn Clock>>,
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
        }
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

    pub fn fail(&self, err: Error) {
        let err = Arc::new(err);
        self.clock.pause();
        self.set_state(PlayerState::Error(err.clone()));
        let _ = self.events.send(PlayerEvent::Error(err));
    }
}

pub struct Player {
    shared: Arc<Shared>,
    commands: Option<Sender<Command>>,
    events: Receiver<PlayerEvent>,
    threads: Vec<JoinHandle<()>>,
    video_size: (u32, u32),
    seekable: bool,
}

impl Player {
    /// Opens the source and probes/creates the demuxer and decoder on the calling thread
    /// (call it off the UI thread), then starts the pipeline threads.
    pub fn open(source: Source, config: PlayerConfig) -> Result<Player> {
        let registry = config.registry.clone().unwrap_or_else(|| Arc::new(Registry::with_defaults()));
        let order = config.backend_order.as_deref();

        let mut src = source.open()?;
        let seekable = src.is_seekable() && !src.is_live();
        let format = demux::probe(src.as_mut())?.ok_or(Error::UnsupportedContainer)?;
        let demuxer = registry.open_demuxer(&source, format, src, order)?;
        let streams = demuxer.streams().to_vec();
        let video = streams
            .iter()
            .find(|s| s.kind == StreamKind::Video)
            .cloned()
            .ok_or(Error::Unsupported("media without a video stream"))?;
        let decoder = registry.open_video_decoder(&video, config.decoder_threads, order)?;

        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        for s in streams.iter().filter(|s| s.kind == StreamKind::Audio) {
            let _ = event_tx.send(PlayerEvent::Warning(format!(
                "audio track {} ({}) ignored: audio playback is not supported yet",
                s.id, s.codec
            )));
        }

        let clock = config.clock.clone().unwrap_or_else(|| Arc::new(SystemClock::new()));
        clock.pause();
        clock.set(Duration::ZERO);
        let shared = Arc::new(Shared {
            state: Mutex::new(PlayerState::Paused),
            clock,
            queue: FrameQueue::new(config.frame_queue_len),
            generation: AtomicU64::new(0),
            ready_generation: AtomicU64::new(u64::MAX),
            wants_play: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            last_frame: Mutex::new(None),
            events: event_tx,
            duration: video.duration,
        });
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let pool = config.thread_pool.clone().unwrap_or_else(shared_thread_pool);
        let threads = pipeline::spawn(
            shared.clone(),
            demuxer,
            decoder,
            video.id,
            cmd_rx,
            config.packet_queue_len,
            pool,
        )?;

        let player = Player {
            shared,
            commands: Some(cmd_tx),
            events: event_rx,
            threads,
            video_size: (video.width, video.height),
            seekable,
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
        s.wants_play.store(true, Ordering::SeqCst);
        if s.ready_generation.load(Ordering::SeqCst) == s.generation.load(Ordering::SeqCst) {
            s.clock.resume();
            s.set_state(PlayerState::Playing);
        } else {
            s.set_state(PlayerState::Buffering);
        }
    }

    pub fn pause(&self) {
        let s = &self.shared;
        if s.state().is_error() {
            return;
        }
        s.wants_play.store(false, Ordering::SeqCst);
        s.clock.pause();
        if s.state() != PlayerState::Ended {
            s.set_state(PlayerState::Paused);
        }
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
        let to = self.shared.duration.map_or(to, |d| to.min(d));
        let generation = s.generation.fetch_add(1, Ordering::SeqCst) + 1;
        s.queue.clear(generation);
        s.clock.pause();
        s.clock.set(to);
        if s.wants_play.load(Ordering::SeqCst) {
            s.set_state(PlayerState::Buffering);
        } else {
            s.set_state(PlayerState::Paused);
        }
        if let Some(tx) = &self.commands {
            let _ = tx.send(Command::Seek { target: to, generation });
        }
    }

    pub fn state(&self) -> PlayerState {
        self.shared.state()
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
        self.commands.take();
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
