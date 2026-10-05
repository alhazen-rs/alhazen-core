//! Displayable frames and the bounded queue between decoder and renderer.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum VideoFrame {
    /// Tightly packed BGRA, `width * 4` bytes per row.
    Cpu { width: u32, height: u32, bgra: Arc<[u8]>, pts: Duration },
    // Phase 4 (macOS): MacSurface(CVPixelBuffer)
}

impl VideoFrame {
    pub fn pts(&self) -> Duration {
        match self {
            VideoFrame::Cpu { pts, .. } => *pts,
        }
    }
    pub fn size(&self) -> (u32, u32) {
        match self {
            VideoFrame::Cpu { width, height, .. } => (*width, *height),
        }
    }
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
mod tests {
    use super::*;
    use std::thread;

    fn frame(ms: u64) -> VideoFrame {
        VideoFrame::Cpu { width: 1, height: 1, bgra: Arc::from(vec![0u8; 4]), pts: Duration::from_millis(ms) }
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
