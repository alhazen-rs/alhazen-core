//! A sliding read cache for non-local sources.
//!
//! Demuxers such as MP4 read samples in decode order, which is not the order they are stored
//! in: audio and video are interleaved in the file, so consecutive packets often seek a few
//! KiB backwards. Over HTTP every such seek outside the source's own cache is a new
//! connection. This wrapper keeps the most recently read [`WINDOW`] bytes and serves backward
//! seeks from memory.

use std::io::{self, Read, Seek, SeekFrom};

use super::MediaSource;

/// Bytes kept behind the read position.
const WINDOW: usize = 1 << 20;
/// Granularity of reads while filling or sliding the window.
const CHUNK: usize = 64 * 1024;

pub(crate) struct CachedSource {
    src: Box<dyn MediaSource>,
    /// `buf[0]` is at this offset in the source.
    buf: Vec<u8>,
    start: u64,
    /// Logical read position.
    pos: u64,
    /// The source's position, when known.
    src_pos: Option<u64>,
    eof: bool,
}

impl CachedSource {
    pub(crate) fn new(src: Box<dyn MediaSource>) -> Self {
        Self { src, buf: Vec::new(), start: 0, pos: 0, src_pos: None, eof: false }
    }

    /// Makes `self.pos` readable, reading from the source only when it is outside the window.
    fn fill(&mut self) -> io::Result<()> {
        let end = self.start + self.buf.len() as u64;
        if self.pos >= self.start && self.pos < end {
            return Ok(());
        }
        if self.eof && self.pos == end {
            return Ok(());
        }
        if self.pos == end && !self.buf.is_empty() {
            // Reading on from the end of the window: slide it instead of seeking.
            if self.buf.len() + CHUNK > WINDOW {
                let drop = self.buf.len() + CHUNK - WINDOW;
                self.buf.drain(..drop);
                self.start += drop as u64;
            }
        } else {
            // A seek outside the window: start a new one at the read position.
            if self.src_pos != Some(self.pos) {
                self.src.seek(SeekFrom::Start(self.pos))?;
                self.src_pos = Some(self.pos);
            }
            self.buf.clear();
            self.start = self.pos;
            self.eof = false;
        }
        while self.buf.len() < WINDOW && !self.eof {
            let old = self.buf.len();
            let want = (old + CHUNK).min(WINDOW);
            self.buf.resize(want, 0);
            let n = match self.src.read(&mut self.buf[old..want]) {
                Ok(n) => n,
                Err(e) => {
                    self.buf.truncate(old);
                    return Err(e);
                }
            };
            self.buf.truncate(old + n);
            if n == 0 {
                self.eof = true;
            } else {
                self.src_pos = Some(self.start + self.buf.len() as u64);
            }
        }
        Ok(())
    }
}

impl Read for CachedSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.fill()?;
        let end = self.start + self.buf.len() as u64;
        if self.pos >= end {
            return Ok(0);
        }
        let off = (self.pos - self.start) as usize;
        let n = buf.len().min(self.buf.len() - off);
        buf[..n].copy_from_slice(&self.buf[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for CachedSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(invalid_seek)?,
            SeekFrom::End(d) => {
                let len = self.byte_len().ok_or_else(|| io::Error::other("length unknown"))?;
                len.checked_add_signed(d).ok_or_else(invalid_seek)?
            }
        };
        self.pos = target;
        Ok(target)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

impl MediaSource for CachedSource {
    fn byte_len(&self) -> Option<u64> {
        self.src.byte_len()
    }
    fn is_seekable(&self) -> bool {
        self.src.is_seekable()
    }
    fn is_live(&self) -> bool {
        self.src.is_live()
    }
    fn description(&self) -> String {
        self.src.description()
    }
    fn is_local(&self) -> bool {
        self.src.is_local()
    }
}

fn invalid_seek() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "seek out of range")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::source::MemorySource;

    /// A source that counts the seeks it receives.
    struct Counting {
        src: MemorySource,
        seeks: Arc<AtomicUsize>,
    }

    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.src.read(buf)
        }
    }

    impl Seek for Counting {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.seeks.fetch_add(1, Ordering::Relaxed);
            self.src.seek(pos)
        }
    }

    impl MediaSource for Counting {
        fn byte_len(&self) -> Option<u64> {
            self.src.byte_len()
        }
        fn is_seekable(&self) -> bool {
            true
        }
        fn is_live(&self) -> bool {
            false
        }
        fn description(&self) -> String {
            "counting".into()
        }
    }

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn cached(data: Vec<u8>) -> (CachedSource, Arc<AtomicUsize>) {
        let seeks = Arc::new(AtomicUsize::new(0));
        let src = Counting { src: MemorySource::new(data, "test"), seeks: seeks.clone() };
        (CachedSource::new(Box::new(src)), seeks)
    }

    #[test]
    fn backward_seek_inside_the_window_does_not_touch_the_source() {
        let bytes = data(2 << 20);
        let (mut src, seeks) = cached(bytes.clone());
        let mut buf = vec![0u8; (3 << 19) + 123]; // past the first window, so it slides
        src.read_exact(&mut buf).unwrap();
        assert_eq!(buf, bytes[..buf.len()]);
        let before = seeks.load(Ordering::Relaxed);
        src.seek(SeekFrom::Start((1 << 20) - 1000)).unwrap();
        let mut back = [0u8; 32];
        src.read_exact(&mut back).unwrap();
        assert_eq!(back, bytes[(1 << 20) - 1000..][..32]);
        assert_eq!(seeks.load(Ordering::Relaxed), before, "a cached seek must not reach the source");
    }

    #[test]
    fn seek_outside_the_window_reads_from_the_source() {
        let bytes = data(2 << 20);
        let (mut src, seeks) = cached(bytes.clone());
        let mut buf = vec![0u8; (3 << 19) + 123];
        src.read_exact(&mut buf).unwrap();
        let before = seeks.load(Ordering::Relaxed);
        src.seek(SeekFrom::Start(0)).unwrap();
        let mut start = [0u8; 16];
        src.read_exact(&mut start).unwrap();
        assert_eq!(start, bytes[..16]);
        assert!(seeks.load(Ordering::Relaxed) > before, "a seek outside the window must reach the source");
    }

    #[test]
    fn sequential_reads_stay_within_the_window_and_reach_the_end() {
        let bytes = data((3 << 20) + 777);
        let (mut src, _) = cached(bytes.clone());
        let mut out = vec![0u8; bytes.len()];
        src.read_exact(&mut out).unwrap();
        assert_eq!(out, bytes);
        assert!(src.buf.len() <= WINDOW, "window grew to {}", src.buf.len());
        assert!(src.buf.capacity() <= WINDOW, "capacity grew to {}", src.buf.capacity());
        assert_eq!(src.read(&mut [0u8; 4]).unwrap(), 0, "end of source");
    }

    #[test]
    fn short_reads_and_seek_from_end() {
        let bytes = data(1000);
        let (mut src, _) = cached(bytes.clone());
        src.seek(SeekFrom::End(-10)).unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(src.read(&mut buf).unwrap(), 10);
        assert_eq!(&buf[..10], &bytes[990..]);
        assert_eq!(src.read(&mut buf).unwrap(), 0, "end of source");
        assert_eq!(src.stream_position().unwrap(), 1000);
        src.seek(SeekFrom::Current(-4)).unwrap();
        src.read_exact(&mut buf[..4]).unwrap();
        assert_eq!(&buf[..4], &bytes[996..]);
    }
}
