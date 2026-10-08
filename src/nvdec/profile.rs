//! Bit depth and chroma format of a video stream, from its container setup data (`avcC`,
//! `hvcC`, `av1C`), so a GPU decoder isn't offered a variant it can't decode.

use crate::demux::{Codec, StreamInfo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chroma {
    Mono,
    Yuv420,
    Yuv422,
    Yuv444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub bit_depth: u32,
    pub chroma: Chroma,
}

/// The stream's format when its setup data says (H.264, HEVC, AV1); `None` otherwise.
pub fn format(stream: &StreamInfo) -> Option<Format> {
    let config = stream.extradata.as_deref()?;
    match stream.codec {
        Codec::H264 => h264(config),
        Codec::Hevc => hevc(config),
        Codec::Av1 => av1(config),
        _ => None,
    }
}

fn chroma(idc: u8) -> Chroma {
    match idc & 3 {
        0 => Chroma::Mono,
        1 => Chroma::Yuv420,
        2 => Chroma::Yuv422,
        _ => Chroma::Yuv444,
    }
}

/// `avcC`: the extension after the parameter sets (High profiles) gives chroma and bit depth;
/// without it, the profile implies them.
fn h264(c: &[u8]) -> Option<Format> {
    let profile = *c.get(1)?;
    // Skip `count` length-prefixed parameter sets starting at `i`.
    let skip = |mut i: usize, count: u8| -> Option<usize> {
        for _ in 0..count {
            i += 2 + u16::from_be_bytes([*c.get(i)?, *c.get(i + 1)?]) as usize;
        }
        Some(i)
    };
    let i = skip(6, c.get(5)? & 0x1F)?; // SPS
    let i = skip(i + 1, *c.get(i)?)?; // PPS
    if matches!(profile, 100 | 110 | 122 | 244) && c.len() >= i + 2 {
        return Some(Format { bit_depth: (c[i + 1] & 7) as u32 + 8, chroma: chroma(c[i]) });
    }
    Some(match profile {
        110 => Format { bit_depth: 10, chroma: Chroma::Yuv420 },
        122 => Format { bit_depth: 10, chroma: Chroma::Yuv422 },
        244 | 44 => Format { bit_depth: 10, chroma: Chroma::Yuv444 },
        _ => Format { bit_depth: 8, chroma: Chroma::Yuv420 },
    })
}

/// `hvcC`: chromaFormat at byte 16, bitDepthLumaMinus8 at byte 17.
fn hevc(c: &[u8]) -> Option<Format> {
    Some(Format { bit_depth: (c.get(17)? & 7) as u32 + 8, chroma: chroma(*c.get(16)?) })
}

/// `av1C`: high_bitdepth, twelve_bit, monochrome and chroma subsampling flags in byte 2.
fn av1(c: &[u8]) -> Option<Format> {
    let b = *c.get(2)?;
    let bit_depth = match (b & 0x40 != 0, b & 0x20 != 0) {
        (false, _) => 8,
        (true, false) => 10,
        (true, true) => 12,
    };
    let chroma = match (b & 0x10 != 0, b & 0x08 != 0, b & 0x04 != 0) {
        (true, ..) => Chroma::Mono,
        (false, true, true) => Chroma::Yuv420,
        (false, true, false) => Chroma::Yuv422,
        _ => Chroma::Yuv444,
    };
    Some(Format { bit_depth, chroma })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "native")]
    fn of(name: &str) -> Option<Format> {
        use crate::demux::{ContainerFormat, Demuxer, MatroskaDemuxer, Mp4Demuxer, StreamKind};
        use crate::source::FileSource;
        let path = format!("tests/fixtures/{name}");
        let mut probe_src = FileSource::open(&path).unwrap();
        let container = crate::demux::probe(&mut probe_src).unwrap().unwrap();
        let src = Box::new(FileSource::open(&path).unwrap());
        let d: Box<dyn Demuxer> = match container {
            ContainerFormat::Mp4 => Box::new(Mp4Demuxer::open(src).unwrap()),
            ContainerFormat::Matroska => Box::new(MatroskaDemuxer::open(src).unwrap()),
        };
        super::format(d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap())
    }

    #[cfg(feature = "native")]
    #[test]
    fn reads_the_fixtures_formats() {
        assert_eq!(of("h264_aac.mp4"), Some(Format { bit_depth: 8, chroma: Chroma::Yuv420 }));
        assert_eq!(of("h264_10bit.mkv"), Some(Format { bit_depth: 10, chroma: Chroma::Yuv420 }));
        assert_eq!(of("hevc.mkv"), Some(Format { bit_depth: 8, chroma: Chroma::Yuv420 }));
        assert_eq!(of("hevc_10bit.mp4"), Some(Format { bit_depth: 10, chroma: Chroma::Yuv420 }));
        assert_eq!(of("hevc_444.mkv"), Some(Format { bit_depth: 8, chroma: Chroma::Yuv444 }));
        assert_eq!(of("av1.webm"), Some(Format { bit_depth: 8, chroma: Chroma::Yuv420 }));
        assert_eq!(of("vp9_444.webm"), None, "VP9 carries no setup data");
    }

    #[test]
    fn h264_profiles_without_the_avcc_extension() {
        // avcC header only (no SPS/PPS, no extension): the profile decides.
        let avcc = |profile: u8| vec![1, profile, 0, 30, 0xFF, 0xE0, 0];
        assert_eq!(h264(&avcc(100)), Some(Format { bit_depth: 8, chroma: Chroma::Yuv420 }));
        assert_eq!(h264(&avcc(110)), Some(Format { bit_depth: 10, chroma: Chroma::Yuv420 }));
        assert_eq!(h264(&avcc(122)).map(|f| f.chroma), Some(Chroma::Yuv422));
        assert_eq!(h264(&avcc(244)).map(|f| f.chroma), Some(Chroma::Yuv444));
        assert_eq!(h264(&[1, 100]), None, "truncated");
    }

    #[test]
    fn av1c_flags() {
        // high_bitdepth, 4:2:0 (subsampling x and y)
        assert_eq!(av1(&[0x81, 0x08, 0b0100_1100, 0]), Some(Format { bit_depth: 10, chroma: Chroma::Yuv420 }));
        // 8-bit 4:4:4 (profile 1)
        assert_eq!(av1(&[0x81, 0x28, 0b0000_0000, 0]), Some(Format { bit_depth: 8, chroma: Chroma::Yuv444 }));
        assert_eq!(av1(&[0x81]), None);
    }
}
