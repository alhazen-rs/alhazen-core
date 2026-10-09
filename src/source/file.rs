use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::MediaSource;
use crate::Result;

/// A local file. Buffering is done by the demuxer, not here.
pub struct FileSource {
    file: File,
    len: u64,
    path: PathBuf,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len, path })
    }
}

impl Read for FileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for FileSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl MediaSource for FileSource {
    fn is_local(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
    fn is_seekable(&self) -> bool {
        true
    }
    fn is_live(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        self.path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_seeks_fixture() {
        let mut src = FileSource::open("tests/fixtures/av1.webm").unwrap();
        assert!(src.byte_len().unwrap() > 1000);
        let mut magic = [0u8; 4];
        src.read_exact(&mut magic).unwrap();
        assert_eq!(magic, [0x1A, 0x45, 0xDF, 0xA3]);
        src.seek(SeekFrom::Start(0)).unwrap();
        src.read_exact(&mut magic).unwrap();
        assert_eq!(magic, [0x1A, 0x45, 0xDF, 0xA3]);
    }

    #[test]
    fn missing_file_is_io_error() {
        assert!(matches!(FileSource::open("nope/missing.webm"), Err(crate::Error::Io(_))));
    }
}
