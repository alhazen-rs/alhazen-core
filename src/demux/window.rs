//! Buffered positional reads over a `MediaSource`: 64 KiB read-ahead, continuing forward reads
//! without seeking (a seek back over HTTP is a new connection, and impossible without Range
//! support).

use std::io::{Read, SeekFrom};

use crate::Result;
use crate::source::MediaSource;

const READ_AHEAD: usize = 64 * 1024;

pub(crate) struct ReadWindow {
    src: Box<dyn MediaSource>,
    buf: Vec<u8>,
    /// File offset of `buf[0]`.
    start: u64,
    /// The source's position when known: `start + buf.len()` after a read.
    src_pos: Option<u64>,
    /// The last read reached the end of the source.
    eof: bool,
}

impl ReadWindow {
    pub fn new(src: Box<dyn MediaSource>) -> Self {
        Self { src, buf: Vec::new(), start: 0, src_pos: None, eof: false }
    }

    pub fn len(&self) -> Option<u64> {
        self.src.byte_len()
    }

    pub fn is_local(&self) -> bool {
        self.src.is_local()
    }

    pub fn is_seekable(&self) -> bool {
        self.src.is_seekable()
    }

    /// Up to `len` bytes at `pos`; fewer only at the end of the file.
    pub fn at(&mut self, pos: u64, len: usize) -> Result<&[u8]> {
        let end = self.start + self.buf.len() as u64;
        let inside = pos >= self.start && pos + len as u64 <= end;
        let continues = pos >= self.start && pos <= end && self.src_pos == Some(end);
        if !inside && !(continues && self.eof) {
            if continues {
                // Keep what is still wanted and read on from where the source is.
                self.buf.drain(..(pos - self.start) as usize);
            } else {
                if self.src_pos != Some(pos) {
                    self.src.seek(SeekFrom::Start(pos))?;
                }
                self.buf.clear();
            }
            self.start = pos;
            let want = len.max(READ_AHEAD);
            let mut filled = self.buf.len();
            self.buf.resize(want.max(filled), 0);
            self.eof = false;
            while filled < want {
                let n = self.src.read(&mut self.buf[filled..])?;
                if n == 0 {
                    self.eof = true;
                    break;
                }
                filled += n;
            }
            self.buf.truncate(filled);
            self.src_pos = Some(self.start + filled as u64);
        }
        let off = (pos - self.start) as usize;
        let end = (off + len).min(self.buf.len());
        Ok(&self.buf[off.min(end)..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileSource;

    #[test]
    fn reads_match_the_file_across_refills_and_at_the_end() {
        let path = "tests/fixtures/pcm_f32le.mkv"; // 94 KB: larger than the read-ahead
        let bytes = std::fs::read(path).unwrap();
        let mut w = ReadWindow::new(Box::new(FileSource::open(path).unwrap()));
        assert_eq!(w.at(0, 10).unwrap(), &bytes[..10]);
        assert_eq!(w.at(70_000, 16).unwrap(), &bytes[70_000..70_016]);
        assert_eq!(w.at(5, 4).unwrap(), &bytes[5..9], "backwards");
        let end = bytes.len() as u64;
        assert_eq!(w.at(end - 5, 16).unwrap(), &bytes[bytes.len() - 5..], "short at the end");
        assert!(w.at(end + 10, 4).unwrap().is_empty());
    }
}
