//! Minimal EBML element reader used by the Matroska demuxer.

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use crate::source::MediaSource;

pub mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const SEGMENT: u32 = 0x1853_8067;
    pub const SEEK_HEAD: u32 = 0x114D_9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
    pub const DURATION: u32 = 0x4489;
    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;
    pub const COLOUR: u32 = 0x55B0;
    pub const MATRIX_COEFFICIENTS: u32 = 0x55B1;
    pub const RANGE: u32 = 0x55B9;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const CHANNELS: u32 = 0x9F;
    pub const BIT_DEPTH: u32 = 0x6264;
    pub const CODEC_DELAY: u32 = 0x56AA;
    pub const SEEK_PRE_ROLL: u32 = 0x56BB;
    pub const FLAG_DEFAULT: u32 = 0x88;
    pub const DEFAULT_DURATION: u32 = 0x23_E383;
    pub const CUES: u32 = 0x1C53_BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const REFERENCE_BLOCK: u32 = 0xFB;
    pub const TITLE: u32 = 0x7BA9;
    pub const TAGS: u32 = 0x1254_C367;
    pub const TAG: u32 = 0x7373;
    pub const SIMPLE_TAG: u32 = 0x67C8;
    pub const TAG_NAME: u32 = 0x45A3;
    pub const TAG_STRING: u32 = 0x4487;
    pub const ATTACHMENTS: u32 = 0x1941_A469;
    pub const ATTACHED_FILE: u32 = 0x61A7;
    pub const FILE_NAME: u32 = 0x466E;
    pub const FILE_MIME_TYPE: u32 = 0x4660;
    pub const FILE_DATA: u32 = 0x465C;
}

/// `size` is `None` for "unknown size" elements (live-written Segments/Clusters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: u32,
    pub size: Option<u64>,
    /// Absolute offset of the element's data (just after the header).
    pub data_start: u64,
}

pub struct EbmlReader {
    inner: BufReader<Box<dyn MediaSource>>,
    seekable: bool,
    pos: u64,
}

impl EbmlReader {
    pub fn new(src: Box<dyn MediaSource>) -> Self {
        let seekable = src.is_seekable();
        Self { inner: BufReader::with_capacity(64 * 1024, src), seekable, pos: 0 }
    }

    pub fn source(&self) -> &dyn MediaSource {
        self.inner.get_ref().as_ref()
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn is_seekable(&self) -> bool {
        self.seekable
    }

    /// Returns `Ok(None)` on a clean EOF at an element boundary.
    pub fn read_header(&mut self) -> io::Result<Option<Header>> {
        if self.inner.fill_buf()?.is_empty() {
            return Ok(None);
        }
        let (id, _) = self.read_vint(4, true)?;
        let (raw, len) = self.read_vint(8, false)?;
        let unknown = raw == (1u64 << (7 * len)) - 1;
        Ok(Some(Header { id: id as u32, size: (!unknown).then_some(raw), data_start: self.pos }))
    }

    /// Reads a variable-length integer. IDs keep their marker bit; sizes do not.
    fn read_vint(&mut self, max_len: u32, keep_marker: bool) -> io::Result<(u64, u32)> {
        let first = self.read_u8()?;
        let len = first.leading_zeros() + 1;
        if len > max_len {
            return Err(invalid("invalid EBML variable-length integer"));
        }
        let mut value = if keep_marker { first as u64 } else { (first as u64) & (0xFF >> len) };
        for _ in 1..len {
            value = (value << 8) | self.read_u8()? as u64;
        }
        Ok((value, len))
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.inner.read_exact(&mut b)?;
        self.pos += 1;
        Ok(b[0])
    }

    pub fn read_bytes(&mut self, size: u64) -> io::Result<Vec<u8>> {
        if size > 256 * 1024 * 1024 {
            return Err(invalid("EBML element too large"));
        }
        let mut buf = vec![0u8; size as usize];
        self.inner.read_exact(&mut buf)?;
        self.pos += size;
        Ok(buf)
    }

    pub fn read_uint(&mut self, size: u64) -> io::Result<u64> {
        if size > 8 {
            return Err(invalid("EBML unsigned int longer than 8 bytes"));
        }
        Ok(self.read_bytes(size)?.iter().fold(0, |acc, b| (acc << 8) | *b as u64))
    }

    pub fn read_float(&mut self, size: u64) -> io::Result<f64> {
        let bytes = self.read_bytes(size)?;
        match size {
            4 => Ok(f32::from_be_bytes(bytes.try_into().unwrap()) as f64),
            8 => Ok(f64::from_be_bytes(bytes.try_into().unwrap())),
            0 => Ok(0.0),
            _ => Err(invalid("EBML float must be 4 or 8 bytes")),
        }
    }

    pub fn read_string(&mut self, size: u64) -> io::Result<String> {
        let bytes = self.read_bytes(size)?;
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    pub fn skip(&mut self, size: u64) -> io::Result<()> {
        if self.seekable {
            self.inner.seek_relative(size as i64)?;
        } else {
            let copied = io::copy(&mut (&mut self.inner).take(size), &mut io::sink())?;
            if copied != size {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        self.pos += size;
        Ok(())
    }

    pub fn seek_to(&mut self, pos: u64) -> io::Result<()> {
        if pos == self.pos {
            return Ok(());
        }
        if !self.seekable {
            if pos > self.pos {
                return self.skip(pos - self.pos);
            }
            return Err(io::Error::new(io::ErrorKind::Unsupported, "source is not seekable"));
        }
        self.inner.seek(SeekFrom::Start(pos))?;
        self.pos = pos;
        Ok(())
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Mem(Cursor<Vec<u8>>);
    impl Read for Mem {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Seek for Mem {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.0.seek(p)
        }
    }
    impl MediaSource for Mem {
        fn byte_len(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
        fn is_seekable(&self) -> bool {
            true
        }
        fn is_live(&self) -> bool {
            false
        }
        fn description(&self) -> String {
            "memory".into()
        }
    }

    fn reader(bytes: &[u8]) -> EbmlReader {
        EbmlReader::new(Box::new(Mem(Cursor::new(bytes.to_vec()))))
    }

    #[test]
    fn reads_header_with_known_and_unknown_size() {
        // TimestampScale (3-byte id) size 3, then Cluster with unknown size (0xFF).
        let mut r = reader(&[0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40, 0x1F, 0x43, 0xB6, 0x75, 0xFF]);
        let h = r.read_header().unwrap().unwrap();
        assert_eq!(h, Header { id: id::TIMESTAMP_SCALE, size: Some(3), data_start: 4 });
        assert_eq!(r.read_uint(3).unwrap(), 1_000_000);
        let h = r.read_header().unwrap().unwrap();
        assert_eq!((h.id, h.size), (id::CLUSTER, None));
        assert_eq!(r.read_header().unwrap(), None);
    }

    #[test]
    fn rejects_invalid_vint() {
        let mut r = reader(&[0x00, 0x00]);
        assert!(r.read_header().is_err());
    }
}
