//! Containers -> packets.

mod ebml;
mod matroska;
#[cfg(feature = "native")]
mod mp4;

use std::io::SeekFrom;
use std::time::Duration;

pub use matroska::MatroskaDemuxer;
#[cfg(feature = "native")]
pub(crate) use matroska::split_xiph_lacing;
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
    /// Apple ProRes (any 422/4444 profile).
    ProRes,
    Opus,
    Vorbis,
    Aac,
    Mp3,
    /// Dolby Digital.
    Ac3,
    /// Dolby Digital Plus.
    Eac3,
    Flac,
    /// Apple Lossless.
    Alac,
    /// Uncompressed PCM audio.
    Pcm(PcmFormat),
    Other(String),
}

/// Layout of uncompressed PCM samples. `bits == 0` means "not stated yet" (Matroska carries the
/// depth in a separate element).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PcmFormat {
    pub bits: u16,
    pub float: bool,
    pub big_endian: bool,
    /// Integer samples only: two's complement (else offset binary, as 8-bit WAV/QuickTime `raw `).
    pub signed: bool,
}

impl PcmFormat {
    pub const fn int(bits: u16, big_endian: bool, signed: bool) -> Self {
        Self { bits, float: false, big_endian, signed }
    }
    pub const fn float(bits: u16, big_endian: bool) -> Self {
        Self { bits, float: true, big_endian, signed: true }
    }
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
            "V_PRORES" => Codec::ProRes,
            "A_OPUS" => Codec::Opus,
            "A_VORBIS" => Codec::Vorbis,
            "A_MPEG/L3" => Codec::Mp3,
            "A_AC3" => Codec::Ac3,
            "A_EAC3" => Codec::Eac3,
            "A_FLAC" => Codec::Flac,
            "A_ALAC" => Codec::Alac,
            // Bit depth comes from the track's BitDepth element; Matroska 8-bit PCM is unsigned.
            "A_PCM/INT/LIT" => Codec::Pcm(PcmFormat::int(0, false, true)),
            "A_PCM/INT/BIG" => Codec::Pcm(PcmFormat::int(0, true, true)),
            "A_PCM/FLOAT/IEEE" => Codec::Pcm(PcmFormat::float(0, false)),
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
            "apco" | "apcs" | "apcn" | "apch" | "ap4h" | "ap4x" => Codec::ProRes,
            "Opus" | "opus" => Codec::Opus,
            "mp4a" => Codec::Aac,
            "ac-3" => Codec::Ac3,
            "ec-3" => Codec::Eac3,
            "fLaC" => Codec::Flac,
            "alac" => Codec::Alac,
            _ => Codec::Other(s.to_owned()),
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Codec::Other(s) => f.write_str(s),
            Codec::Pcm(p) => {
                let kind = if p.float { "f" } else if p.signed { "s" } else { "u" };
                write!(f, "PCM {kind}{}{}", p.bits, if p.big_endian { "be" } else { "le" })
            }
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
    /// Container "default track" flag (true when the container has no such flag).
    pub default: bool,
    /// Audio only: samples per second and channel count (0 when unknown).
    pub sample_rate: u32,
    pub channels: u16,
    /// Audio only: decoder delay to discard at the start (Matroska CodecDelay).
    pub codec_delay: Duration,
    /// Audio only: how much earlier decoding must start for a seek to be clean (SeekPreRoll).
    pub seek_preroll: Duration,
    /// Video only: colour matrix as signalled by the container (ITU-T H.273 MatrixCoefficients:
    /// 1 BT.709, 5/6 BT.601, 9/10 BT.2020), when present.
    pub color_matrix: Option<u8>,
    /// Video only: whether samples use the full range (container-signalled), when present.
    pub full_range: Option<bool>,
}

impl StreamInfo {
    /// A stream with every optional field empty.
    pub fn new(id: u32, kind: StreamKind, codec: Codec) -> Self {
        Self {
            id,
            kind,
            codec,
            width: 0,
            height: 0,
            duration: None,
            extradata: None,
            default: true,
            sample_rate: 0,
            channels: 0,
            codec_delay: Duration::ZERO,
            seek_preroll: Duration::ZERO,
            color_matrix: None,
            full_range: None,
        }
    }
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
    // QuickTime files may start with `wide`/`mdat`/`free` before `moov`.
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"styp" | b"wide" | b"mdat" | b"free") {
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
        assert_eq!(probe_path("tests/fixtures/prores_hq.mov"), Some(ContainerFormat::Mp4));
        assert_eq!(probe_path("tests/fixtures/not_video.bin"), None);
    }

    #[test]
    fn maps_codec_ids() {
        assert_eq!(Codec::from_matroska_id("V_AV1"), Codec::Av1);
        assert_eq!(Codec::from_matroska_id("A_AAC/MPEG4/LC"), Codec::Aac);
        assert_eq!(Codec::from_mp4_codec_string("av01.0.00M.08"), Codec::Av1);
        assert_eq!(Codec::from_mp4_codec_string("avc1.64001f"), Codec::H264);
        assert_eq!(Codec::from_mp4_codec_string("xyz1"), Codec::Other("xyz1".into()));
        assert_eq!(Codec::from_matroska_id("V_PRORES"), Codec::ProRes);
        assert_eq!(Codec::from_matroska_id("A_MPEG/L3"), Codec::Mp3);
        assert_eq!(Codec::from_matroska_id("A_AC3"), Codec::Ac3);
        assert_eq!(Codec::from_matroska_id("A_EAC3"), Codec::Eac3);
        assert_eq!(Codec::from_matroska_id("A_FLAC"), Codec::Flac);
        assert_eq!(Codec::from_matroska_id("A_ALAC"), Codec::Alac);
        assert_eq!(Codec::from_mp4_codec_string("ac-3"), Codec::Ac3);
        assert_eq!(Codec::from_mp4_codec_string("ec-3"), Codec::Eac3);
        assert_eq!(Codec::from_mp4_codec_string("fLaC"), Codec::Flac);
        assert_eq!(Codec::from_mp4_codec_string("alac"), Codec::Alac);
        assert_eq!(Codec::from_mp4_codec_string("ap4h"), Codec::ProRes);
        assert_eq!(Codec::Vp9.to_string(), "Vp9");
    }
}
