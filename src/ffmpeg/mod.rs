//! The `ffmpeg-cli` backend: decoding through the user's own `ffmpeg` program.
//!
//! The crate never links ffmpeg. It runs the `ffmpeg` binary it finds as a pure decoder: our
//! demuxers feed it packets as a Matroska stream on stdin, and it writes raw pictures or samples
//! to stdout. Which codecs work, whether hardware decoding is used, and the patent licensing of
//! that binary are properties of the user's ffmpeg installation, not of this crate.

use std::path::PathBuf;

/// How the `ffmpeg-cli` backend finds and runs ffmpeg.
#[derive(Clone, Debug)]
pub struct FfmpegConfig {
    /// Register the backend at all.
    pub enabled: bool,
    /// The `ffmpeg` program. `None`: `$VIDEO_CORE_FFMPEG`, else `ffmpeg` on `PATH`, and on macOS
    /// also Homebrew's and MacPorts' install locations (apps launched from Finder don't get the
    /// shell's `PATH`).
    pub path: Option<PathBuf>,
    /// Pass `-hwaccel auto` so ffmpeg decodes on the GPU when it can.
    pub hwaccel: bool,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self { enabled: true, path: None, hwaccel: true }
    }
}

#[cfg(feature = "ffmpeg-cli")]
mod audio;
#[cfg(feature = "ffmpeg-cli")]
pub mod locate;
#[cfg(feature = "ffmpeg-cli")]
pub mod mkv;
#[cfg(feature = "ffmpeg-cli")]
pub(crate) mod process;
#[cfg(feature = "ffmpeg-cli")]
mod video;

#[cfg(feature = "ffmpeg-cli")]
pub use audio::FfmpegAudioDecoder;
#[cfg(feature = "ffmpeg-cli")]
pub use backend::FfmpegCliBackend;
#[cfg(feature = "ffmpeg-cli")]
pub use video::FfmpegVideoDecoder;

#[cfg(feature = "ffmpeg-cli")]
mod backend {
    use std::sync::{Arc, OnceLock};

    use super::FfmpegConfig;
    use super::locate::{FfmpegInfo, find};
    use crate::backend::Backend;
    use crate::decode::{AudioDecoder, VideoDecoder};
    use crate::demux::{Codec, ContainerFormat, Demuxer, StreamInfo, StreamKind};
    use crate::source::MediaSource;
    use crate::{Error, Result};

    /// Decodes through the user's `ffmpeg` program. Tried after the native decoders.
    pub struct FfmpegCliBackend {
        config: FfmpegConfig,
        info: OnceLock<Option<Arc<FfmpegInfo>>>,
    }

    impl FfmpegCliBackend {
        pub fn new(config: FfmpegConfig) -> Self {
            Self { config, info: OnceLock::new() }
        }

        /// The located ffmpeg, probed on first use (never on the open path of natively
        /// decodable media).
        pub fn ffmpeg(&self) -> Option<Arc<FfmpegInfo>> {
            self.info.get_or_init(|| find(self.config.path.as_deref())).clone()
        }

        fn supports(&self, stream: &StreamInfo, kind: StreamKind) -> bool {
            stream.kind == kind
                && super::mkv::matroska_codec_id(stream).is_some()
                && self.ffmpeg().is_some_and(|f| f.has_decoder(decoder_names(&stream.codec)))
        }
    }

    impl Backend for FfmpegCliBackend {
        fn name(&self) -> &'static str {
            "ffmpeg-cli"
        }
        fn priority(&self) -> i32 {
            -10
        }
        fn supports_container(&self, _format: ContainerFormat) -> bool {
            false
        }
        fn open_demuxer(&self, _format: ContainerFormat, _src: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>> {
            Err(Error::Unsupported("ffmpeg-cli demuxing"))
        }
        fn supports_video(&self, stream: &StreamInfo) -> bool {
            self.supports(stream, StreamKind::Video)
        }
        fn open_video_decoder(&self, stream: &StreamInfo, _threads: usize) -> Result<Box<dyn VideoDecoder>> {
            let info = self.ffmpeg().ok_or(Error::Unsupported("ffmpeg not found"))?;
            Ok(Box::new(super::FfmpegVideoDecoder::new(info, stream, self.config.hwaccel)?))
        }
        fn supports_audio(&self, stream: &StreamInfo) -> bool {
            self.supports(stream, StreamKind::Audio)
        }
        fn open_audio_decoder(&self, stream: &StreamInfo) -> Result<Box<dyn AudioDecoder>> {
            let info = self.ffmpeg().ok_or(Error::Unsupported("ffmpeg not found"))?;
            Ok(Box::new(super::FfmpegAudioDecoder::new(info, stream)?))
        }
    }

    /// ffmpeg decoder names that handle `codec` (any one suffices).
    pub fn decoder_names(codec: &Codec) -> &'static [&'static str] {
        match codec {
            Codec::Av1 => &["libdav1d", "av1", "libaom-av1"],
            Codec::Vp8 => &["vp8", "libvpx"],
            Codec::Vp9 => &["vp9", "libvpx-vp9"],
            Codec::H264 => &["h264"],
            Codec::Hevc => &["hevc"],
            Codec::ProRes => &["prores"],
            Codec::Opus => &["opus", "libopus"],
            Codec::Vorbis => &["vorbis", "libvorbis"],
            Codec::Aac => &["aac", "aac_fixed", "libfdk_aac"],
            Codec::Other(id) => match id.as_str() {
                "V_MPEG1" => &["mpeg1video"],
                "V_MPEG2" => &["mpeg2video"],
                "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" => &["mpeg4"],
                "V_THEORA" => &["theora"],
                "A_AC3" => &["ac3", "ac3_fixed"],
                "A_EAC3" => &["eac3"],
                "A_DTS" => &["dca"],
                "A_FLAC" => &["flac"],
                "A_MPEG/L3" => &["mp3", "mp3float"],
                "A_MPEG/L2" => &["mp2", "mp2float"],
                "A_TRUEHD" => &["truehd"],
                "A_ALAC" => &["alac"],
                _ => &[],
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nonexistent_ffmpeg_claims_nothing() {
            let b = FfmpegCliBackend::new(FfmpegConfig {
                path: Some("/nonexistent/ffmpeg-for-video-core-tests".into()),
                ..FfmpegConfig::default()
            });
            let h264 = StreamInfo::new(1, StreamKind::Video, Codec::H264);
            assert!(!b.supports_video(&h264));
            assert!(b.open_video_decoder(&h264, 1).is_err());
        }

        #[test]
        fn every_modelled_codec_has_decoder_names() {
            for c in [Codec::Av1, Codec::Vp8, Codec::Vp9, Codec::H264, Codec::Hevc, Codec::ProRes, Codec::Opus, Codec::Vorbis, Codec::Aac] {
                assert!(!decoder_names(&c).is_empty(), "{c}");
            }
            assert!(decoder_names(&Codec::Other("V_UNKNOWN".into())).is_empty());
        }
    }
}
