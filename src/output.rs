//! What the decoders should produce, from the formats the app can draw.

use crate::demux::StreamInfo;
use crate::frame::FrameFormat;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct OutputPrefs {
    /// The decoder may emit `DecodedFrame::Bgra`: converted (SDR, tone mapped) and scaled on the GPU.
    pub gpu_bgra: bool,
    /// macOS: the decoder may emit `DecodedFrame::Native` for SDR sources.
    pub native: bool,
    /// Keep 10-bit samples (`YuvPlanes::U16`) instead of shifting them to 8 bits.
    pub ten_bit: bool,
}

impl OutputPrefs {
    pub fn for_stream(formats: &[FrameFormat], stream: &StreamInfo) -> OutputPrefs {
        OutputPrefs {
            gpu_bgra: !planes_first(formats),
            native: cfg!(target_os = "macos") && formats.contains(&FrameFormat::Native),
            ten_bit: formats.contains(&FrameFormat::Yuv10) || hdr_source(stream),
        }
    }
}

/// Whether the app wants YUV planes rather than BGRA (the first non-native format it lists).
pub(crate) fn planes_first(formats: &[FrameFormat]) -> bool {
    formats
        .iter()
        .find(|f| **f != FrameFormat::Native)
        .is_some_and(|f| matches!(f, FrameFormat::Yuv | FrameFormat::Yuv10))
}

/// Whether the stream is PQ/HLG.
fn hdr_source(_stream: &StreamInfo) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Codec, StreamKind};

    fn video() -> StreamInfo {
        let mut s = StreamInfo::new(0, StreamKind::Video, Codec::Hevc);
        s.width = 3840;
        s.height = 2160;
        s
    }

    #[test]
    fn default_bgra_wants_gpu_bgra_8bit() {
        let p = OutputPrefs::for_stream(&[FrameFormat::Bgra], &video());
        assert_eq!(p, OutputPrefs { gpu_bgra: true, native: false, ten_bit: false });
    }

    #[test]
    fn yuv10_first_keeps_ten_bit_and_planes() {
        let p = OutputPrefs::for_stream(&[FrameFormat::Yuv10, FrameFormat::Bgra], &video());
        assert!(p.ten_bit && !p.gpu_bgra);
    }

    #[test]
    fn native_is_macos_only() {
        let p = OutputPrefs::for_stream(&[FrameFormat::Native, FrameFormat::Bgra], &video());
        assert_eq!(p.native, cfg!(target_os = "macos"));
        assert!(p.gpu_bgra);
    }
}
