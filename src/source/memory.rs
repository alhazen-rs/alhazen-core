use std::io::{self, Cursor, Read, Seek, SeekFrom};

use super::MediaSource;

/// Bytes already in memory (an HLS segment).
pub struct MemorySource {
    data: Cursor<Vec<u8>>,
    description: String,
}

impl MemorySource {
    pub fn new(data: Vec<u8>, description: impl Into<String>) -> Self {
        Self { data: Cursor::new(data), description: description.into() }
    }
}

impl Read for MemorySource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.data.read(buf)
    }
}

impl Seek for MemorySource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.data.seek(pos)
    }
}

impl MediaSource for MemorySource {
    fn is_local(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.data.get_ref().len() as u64)
    }
    fn is_seekable(&self) -> bool {
        true
    }
    fn is_live(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        self.description.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_seeks() {
        let mut src = MemorySource::new((0..10).collect(), "segment");
        assert_eq!(src.byte_len(), Some(10));
        let mut buf = [0u8; 3];
        src.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [0, 1, 2]);
        src.seek(SeekFrom::End(-2)).unwrap();
        assert_eq!(src.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[8, 9]);
        assert_eq!(src.read(&mut buf).unwrap(), 0, "end");
        src.seek(SeekFrom::Start(4)).unwrap();
        src.seek(SeekFrom::Current(1)).unwrap();
        src.read_exact(&mut buf[..1]).unwrap();
        assert_eq!(buf[0], 5);
    }
}
