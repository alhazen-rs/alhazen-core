//! Containers -> packets.

mod ebml;
mod matroska;
#[cfg(feature = "native")]
mod mp4;

use std::io::SeekFrom;
use std::time::Duration;

pub use matroska::MatroskaDemuxer;
#[cfg(feature = "native")]
pub use mp4::Mp4Demuxer;

use crate::Result;
use crate::source::MediaSource;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    Av1,
    Vp8,
    Vp9,
    H264,
    Hevc,
    Opus,
    Vorbis,
    Aac,
    Other(String),
}

impl Codec {
    /// Maps a Matroska `CodecID` (e.g. `V_AV1`).
    pub fn from_matroska_id(id: &str) -> Codec {
        match id {
            "V_AV1" => Codec::Av1,
            "V_VP8" => Codec::Vp8,
            "V_VP9" => Codec::Vp9,
            "V_MPEG4/ISO/AVC" => Codec::H264,
            "V_MPEGH/ISO/HEVC" => Codec::Hevc,
            "A_OPUS" => Codec::Opus,
            "A_VORBIS" => Codec::Vorbis,
            id if id.starts_with("A_AAC") => Codec::Aac,
            other => Codec::Other(other.to_owned()),
        }
    }

    /// Maps an RFC 6381 codec string as produced by MP4 parsers (e.g. `av01.0.00M.08`).
    pub fn from_mp4_codec_string(s: &str) -> Codec {
        let fourcc = s.split('.').next().unwrap_or(s);
        match fourcc {
            "av01" => Codec::Av1,
            "vp08" => Codec::Vp8,
            "vp09" => Codec::Vp9,
            "avc1" | "avc3" => Codec::H264,
            "hvc1" | "hev1" => Codec::Hevc,
            "Opus" | "opus" => Codec::Opus,
            "mp4a" => Codec::Aac,
            _ => Codec::Other(s.to_owned()),
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Codec::Other(s) => f.write_str(s),
            known => write!(f, "{known:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    /// Container-level track id; matches `Packet::stream`.
    pub id: u32,
    pub kind: StreamKind,
    pub codec: Codec,
    /// Video only.
    pub width: u32,
    pub height: u32,
    pub duration: Option<Duration>,
    /// Codec-specific setup data (Matroska CodecPrivate / MP4 sample entry config).
    pub extradata: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub stream: u32,
    pub pts: Duration,
    pub keyframe: bool,
    pub data: Vec<u8>,
    /// Set by the pipeline, not the demuxer: the seek generation this packet belongs to.
    pub generation: u64,
}

pub trait Demuxer: Send {
    fn streams(&self) -> &[StreamInfo];
    /// `Ok(None)` at end of stream.
    fn next_packet(&mut self) -> Result<Option<Packet>>;
    /// Repositions so the next video packet is a keyframe at or before `target`.
    /// Returns that keyframe's timestamp (or the closest position reached).
    fn seek(&mut self, target: Duration) -> Result<Duration>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerFormat {
    Matroska,
    Mp4,
}

/// Detects the container from magic bytes, then rewinds.
pub fn probe(src: &mut dyn MediaSource) -> Result<Option<ContainerFormat>> {
    let mut head = [0u8; 12];
    let mut filled = 0;
    while filled < head.len() {
        let n = src.read(&mut head[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    src.seek(SeekFrom::Start(0))?;
    let head = &head[..filled];
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Ok(Some(ContainerFormat::Matroska));
    }
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"styp") {
        return Ok(Some(ContainerFormat::Mp4));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileSource;

    fn probe_path(p: &str) -> Option<ContainerFormat> {
        let mut src = FileSource::open(p).unwrap();
        probe(&mut src).unwrap()
    }

    #[test]
    fn probes_by_magic_bytes() {
        assert_eq!(probe_path("tests/fixtures/av1.webm"), Some(ContainerFormat::Matroska));
        assert_eq!(probe_path("tests/fixtures/av1.mp4"), Some(ContainerFormat::Mp4));
        assert_eq!(probe_path("tests/fixtures/not_video.bin"), None);
    }

    #[test]
    fn maps_codec_ids() {
        assert_eq!(Codec::from_matroska_id("V_AV1"), Codec::Av1);
        assert_eq!(Codec::from_matroska_id("A_AAC/MPEG4/LC"), Codec::Aac);
        assert_eq!(Codec::from_mp4_codec_string("av01.0.00M.08"), Codec::Av1);
        assert_eq!(Codec::from_mp4_codec_string("avc1.64001f"), Codec::H264);
        assert_eq!(Codec::from_mp4_codec_string("xyz1"), Codec::Other("xyz1".into()));
        assert_eq!(Codec::Vp9.to_string(), "Vp9");
    }
}
