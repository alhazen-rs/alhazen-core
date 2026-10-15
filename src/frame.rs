//! Displayable frames and the bounded queue between decoder and renderer.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::decode::YuvFrame;

#[derive(Clone, Debug)]
pub enum VideoFrame {
    /// Tightly packed BGRA, `width * 4` bytes per row. Always SDR.
    Bgra { width: u32, height: u32, bgra: Arc<[u8]>, pts: Duration },
    /// Planar YUV: 8-bit, or 10-bit when `FrameFormat::Yuv10` was asked for.
    Yuv(Arc<YuvFrame>),
    /// macOS: VideoToolbox's pixel buffer, for drawing without a copy (`FrameFormat::Native`).
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    Native(crate::apple::NativeFrame),
}

impl VideoFrame {
    pub fn pts(&self) -> Duration {
        match self {
            VideoFrame::Bgra { pts, .. } => *pts,
            VideoFrame::Yuv(f) => f.pts,
            #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
            VideoFrame::Native(f) => f.pts,
        }
    }
    pub fn size(&self) -> (u32, u32) {
        match self {
            VideoFrame::Bgra { width, height, .. } => (*width, *height),
            VideoFrame::Yuv(f) => (f.width, f.height),
            #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
            VideoFrame::Native(f) => (f.width, f.height),
        }
    }
}

/// Buffers for pictures coming back from a GPU, reused once the renderer has let go of them.
/// A 4K BGRA picture is 33 MB: allocating one per frame costs thousands of page faults each
/// time, while refilling memory already mapped is a plain copy.
#[derive(Default)]
#[cfg_attr(
    not(any(
        all(target_os = "linux", feature = "nvdec"),
        all(windows, feature = "media-foundation"),
        all(target_os = "macos", feature = "videotoolbox")
    )),
    allow(dead_code)
)]
pub(crate) struct FramePool {
    buffers: VecDeque<Arc<[u8]>>,
}

#[cfg_attr(
    not(any(
        all(target_os = "linux", feature = "nvdec"),
        all(windows, feature = "media-foundation"),
        all(target_os = "macos", feature = "videotoolbox")
    )),
    allow(dead_code)
)]
impl FramePool {
    /// Buffers kept for reuse (the frame queue and the renderer hold a few at a time).
    const KEEP: usize = 8;

    /// A `len`-byte buffer written by `write`: a released one of that size when there is one.
    pub fn fill(&mut self, len: usize, write: impl FnOnce(&mut [u8])) -> Arc<[u8]> {
        if let Some(i) = self.buffers.iter().position(|b| b.len() == len && Arc::strong_count(b) == 1) {
            let mut buffer = self.buffers.remove(i).expect("found above");
            write(Arc::get_mut(&mut buffer).expect("only the pool holds it"));
            self.buffers.push_back(buffer.clone());
            return buffer;
        }
        let buffer = Arc::<[u8]>::new_zeroed_slice(len);
        // SAFETY: zeroed bytes are valid `u8`s.
        let mut buffer = unsafe { buffer.assume_init() };
        write(Arc::get_mut(&mut buffer).expect("just allocated"));
        if self.buffers.len() >= Self::KEEP {
            self.buffers.pop_front();
        }
        self.buffers.push_back(buffer.clone());
        buffer
    }
}

/// A frame format the renderer can draw (`PlayerConfig::frame_formats`, most preferred first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    /// BGRA, always SDR; always available as the fallback.
    Bgra,
    /// 8-bit planar YUV with its colour description.
    Yuv,
    /// Planar YUV keeping 10-bit samples (16-bit planes) and HDR metadata.
    Yuv10,
    /// macOS: VideoToolbox's own pixel buffer, for zero-copy drawing (SDR sources only).
    Native,
}

struct Inner {
    frames: VecDeque<(u64, VideoFrame)>,
    generation: u64,
    closed: bool,
}

/// Bounded frame queue. The decode thread blocks in `push` when full (back-pressure);
/// the renderer never blocks.
pub struct FrameQueue {
    inner: Mutex<Inner>,
    space: Condvar,
    capacity: usize,
    /// Frames discarded by `frame_for` because a later one was already due.
    dropped: AtomicU64,
    /// Frames the decode side never converted because they were already late (catch-up).
    skipped: AtomicU64,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner { frames: VecDeque::new(), generation: 0, closed: false }),
            space: Condvar::new(),
            capacity: capacity.max(1),
            dropped: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }
    }

    /// Blocks while full. Returns `false` (and drops the frame) if the queue was closed or the
    /// frame belongs to an older generation than the queue.
    pub fn push(&self, generation: u64, frame: VideoFrame) -> bool {
        let mut q = self.inner.lock().unwrap();
        loop {
            if q.closed || generation < q.generation {
                return false;
            }
            if q.frames.len() < self.capacity {
                q.frames.push_back((generation, frame));
                return true;
            }
            q = self.space.wait(q).unwrap();
        }
    }

    /// The latest frame with `pts <= now`, discarding older (late) frames.
    /// Returns `None` if no frame is due yet.
    pub fn frame_for(&self, now: Duration) -> Option<VideoFrame> {
        let mut q = self.inner.lock().unwrap();
        let mut due = None;
        while q.frames.front().is_some_and(|(_, f)| f.pts() <= now) {
            if due.is_some() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            due = q.frames.pop_front().map(|(_, f)| f);
        }
        if due.is_some() {
            self.space.notify_all();
        }
        due
    }

    /// Counts a frame the decode side discarded because it was already late.
    pub fn note_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// How many frames the decode side skipped while catching up.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// How many frames were never shown because they were already late.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Pts of the next queued frame, if any.
    pub fn peek_pts(&self) -> Option<Duration> {
        self.inner.lock().unwrap().frames.front().map(|(_, f)| f.pts())
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops all frames and rejects future pushes from generations older than `generation`.
    pub fn clear(&self, generation: u64) {
        let mut q = self.inner.lock().unwrap();
        q.generation = generation;
        q.frames.clear();
        self.space.notify_all();
    }

    /// Wakes and rejects all pushers; used on shutdown.
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.space.notify_all();
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    fn fill(pool: &mut FramePool, len: usize, byte: u8) -> Arc<[u8]> {
        pool.fill(len, |dst| dst.fill(byte))
    }

    #[test]
    fn a_released_buffer_is_reused() {
        let mut pool = FramePool::default();
        let a = fill(&mut pool, 64, 1);
        let at = a.as_ptr();
        drop(a);
        let b = fill(&mut pool, 64, 2);
        assert_eq!(b.as_ptr(), at, "the released buffer is filled again");
        assert!(b.iter().all(|&x| x == 2));
    }

    #[test]
    fn a_buffer_still_held_is_never_overwritten() {
        let mut pool = FramePool::default();
        let held: Vec<_> = (0..10).map(|i| fill(&mut pool, 64, i)).collect();
        for (i, h) in held.iter().enumerate() {
            assert!(h.iter().all(|&x| x == i as u8), "buffer {i} was overwritten");
        }
    }

    #[test]
    fn only_buffers_of_the_same_size_are_reused() {
        let mut pool = FramePool::default();
        let a = fill(&mut pool, 64, 1);
        let at = a.as_ptr();
        drop(a);
        let b = fill(&mut pool, 128, 2);
        assert_eq!(b.len(), 128);
        assert_ne!(b.as_ptr(), at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn frame(ms: u64) -> VideoFrame {
        VideoFrame::Bgra { width: 1, height: 1, bgra: Arc::from(vec![0u8; 4]), pts: Duration::from_millis(ms) }
    }

    #[test]
    fn returns_latest_due_frame_and_drops_late_ones() {
        let q = FrameQueue::new(8);
        for ms in [0, 33, 66, 100] {
            assert!(q.push(0, frame(ms)));
        }
        assert_eq!(q.frame_for(Duration::from_millis(70)).unwrap().pts(), Duration::from_millis(66));
        assert_eq!(q.len(), 1);
        assert_eq!(q.dropped(), 2, "0 and 33 were never shown");
        assert!(q.frame_for(Duration::from_millis(80)).is_none());
        assert_eq!(q.peek_pts(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn push_blocks_when_full_until_space_frees() {
        let q = Arc::new(FrameQueue::new(1));
        q.push(0, frame(0));
        let q2 = q.clone();
        let pusher = thread::spawn(move || q2.push(0, frame(33)));
        thread::sleep(Duration::from_millis(50));
        assert!(!pusher.is_finished(), "push must block while full");
        q.frame_for(Duration::from_millis(0));
        assert!(pusher.join().unwrap());
    }

    #[test]
    fn clear_rejects_stale_generation() {
        let q = FrameQueue::new(4);
        q.push(0, frame(0));
        q.clear(1);
        assert!(q.is_empty());
        assert!(!q.push(0, frame(10)), "old generation is rejected");
        assert!(q.push(1, frame(20)));
    }

    #[test]
    fn close_unblocks_pusher() {
        let q = Arc::new(FrameQueue::new(1));
        q.push(0, frame(0));
        let q2 = q.clone();
        let pusher = thread::spawn(move || q2.push(0, frame(33)));
        thread::sleep(Duration::from_millis(20));
        q.close();
        assert!(!pusher.join().unwrap());
    }
}
