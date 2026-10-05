//! Backend registry: which demuxers/decoders exist, and in which order to try them.

use std::sync::Arc;

use crate::decode::{AudioDecoder, VideoDecoder};
use crate::demux::{ContainerFormat, Demuxer, StreamInfo};
use crate::source::{MediaSource, Source};
use crate::{Error, Result};

pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    /// Higher is tried first (unless `PlayerConfig::backend_order` says otherwise).
    fn priority(&self) -> i32;
    fn supports_container(&self, format: ContainerFormat) -> bool;
    fn open_demuxer(&self, format: ContainerFormat, src: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>>;
    fn supports_video(&self, stream: &StreamInfo) -> bool;
    fn open_video_decoder(&self, stream: &StreamInfo, threads: usize) -> Result<Box<dyn VideoDecoder>>;
    /// Audio support is optional for a backend.
    fn supports_audio(&self, _stream: &StreamInfo) -> bool {
        false
    }
    fn open_audio_decoder(&self, _stream: &StreamInfo) -> Result<Box<dyn AudioDecoder>> {
        Err(Error::Unsupported("audio"))
    }
}

#[derive(Clone, Default)]
pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
}

impl Registry {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every backend compiled in through Cargo features, ffmpeg found the default way.
    pub fn with_defaults() -> Self {
        Self::with_ffmpeg(&crate::FfmpegConfig::default())
    }

    /// Every backend compiled in through Cargo features, with this ffmpeg configuration.
    pub fn with_ffmpeg(ffmpeg: &crate::FfmpegConfig) -> Self {
        #[cfg_attr(not(any(feature = "native", feature = "ffmpeg-cli")), allow(unused_mut))]
        let mut r = Self::empty();
        #[cfg(feature = "native")]
        r.register(Arc::new(NativeBackend));
        #[cfg(feature = "ffmpeg-cli")]
        if ffmpeg.enabled {
            r.register(Arc::new(crate::ffmpeg::FfmpegCliBackend::new(ffmpeg.clone())));
        }
        #[cfg(not(feature = "ffmpeg-cli"))]
        let _ = ffmpeg;
        r
    }

    pub fn register(&mut self, backend: Arc<dyn Backend>) {
        self.backends.push(backend);
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    /// Backends named in `order` first (in that order), then the rest by descending priority.
    pub fn ordered(&self, order: Option<&[&'static str]>) -> Vec<Arc<dyn Backend>> {
        let mut rest: Vec<_> = self.backends.clone();
        rest.sort_by_key(|b| std::cmp::Reverse(b.priority()));
        let mut out = Vec::new();
        for name in order.unwrap_or_default() {
            if let Some(i) = rest.iter().position(|b| b.name() == *name) {
                out.push(rest.remove(i));
            }
        }
        out.extend(rest);
        out
    }

    /// Opens a demuxer, trying each backend that claims the container; falls back on failure.
    pub fn open_demuxer(
        &self,
        source: &Source,
        format: ContainerFormat,
        first_src: Box<dyn MediaSource>,
        order: Option<&[&'static str]>,
    ) -> Result<Box<dyn Demuxer>> {
        let mut src = Some(first_src);
        let mut last_err = None;
        for b in self.ordered(order).into_iter().filter(|b| b.supports_container(format)) {
            let s = match src.take() {
                Some(s) => s,
                None => source.open()?,
            };
            match b.open_demuxer(format, s) {
                Ok(d) => return Ok(d),
                Err(e) => {
                    log::warn!("backend {} failed to open {format:?}: {e}", b.name());
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or(Error::UnsupportedContainer))
    }

    /// Opens an audio decoder for `stream`, trying each capable backend in order.
    pub fn open_audio_decoder(&self, stream: &StreamInfo, order: Option<&[&'static str]>) -> Result<Box<dyn AudioDecoder>> {
        let mut tried = Vec::new();
        let mut last_err = None;
        for b in self.ordered(order).into_iter().filter(|b| b.supports_audio(stream)) {
            tried.push(b.name());
            match b.open_audio_decoder(stream) {
                Ok(d) => return Ok(d),
                Err(e) => {
                    log::warn!("backend {} failed to open {} decoder: {e}", b.name(), stream.codec);
                    last_err = Some(e);
                }
            }
        }
        // A decoder that exists but refused this stream says more than "unsupported codec".
        Err(last_err.unwrap_or(Error::UnsupportedCodec { codec: stream.codec.to_string(), tried_backends: tried }))
    }

    /// Opens a video decoder for `stream`, trying each capable backend in order.
    pub fn open_video_decoder(
        &self,
        stream: &StreamInfo,
        threads: usize,
        order: Option<&[&'static str]>,
    ) -> Result<Box<dyn VideoDecoder>> {
        self.open_video_decoder_except(stream, threads, order, None).map(|(_, d)| d)
    }

    /// Like `open_video_decoder`, skipping the backend named `except`; also returns the name of
    /// the backend that opened it. Used to find a faster decoder when the current one is too slow.
    pub fn open_video_decoder_except(
        &self,
        stream: &StreamInfo,
        threads: usize,
        order: Option<&[&'static str]>,
        except: Option<&str>,
    ) -> Result<(&'static str, Box<dyn VideoDecoder>)> {
        let mut tried = Vec::new();
        let candidates = self.ordered(order).into_iter().filter(|b| Some(b.name()) != except);
        for b in candidates.filter(|b| b.supports_video(stream)) {
            tried.push(b.name());
            match b.open_video_decoder(stream, threads) {
                Ok(d) => return Ok((b.name(), d)),
                Err(e) => log::warn!("backend {} failed to open {} decoder: {e}", b.name(), stream.codec),
            }
        }
        Err(Error::UnsupportedCodec { codec: stream.codec.to_string(), tried_backends: tried })
    }
}

/// Pure-Rust backend: Matroska/WebM + MP4/MOV demuxing; AV1 (rav1d), VP9 (vp9-mt),
/// VP8 (oximedia-codec) and ProRes (oxideav-prores) decoding.
#[cfg(feature = "native")]
pub struct NativeBackend;

#[cfg(feature = "native")]
impl Backend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }
    fn priority(&self) -> i32 {
        0
    }
    fn supports_container(&self, _format: ContainerFormat) -> bool {
        true
    }
    fn open_demuxer(&self, format: ContainerFormat, src: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>> {
        Ok(match format {
            ContainerFormat::Matroska => Box::new(crate::demux::MatroskaDemuxer::open(src)?),
            ContainerFormat::Mp4 => Box::new(crate::demux::Mp4Demuxer::open(src)?),
        })
    }
    fn supports_video(&self, stream: &StreamInfo) -> bool {
        use crate::demux::Codec;
        matches!(stream.codec, Codec::Av1 | Codec::Vp9 | Codec::Vp8 | Codec::ProRes)
    }
    fn open_video_decoder(&self, stream: &StreamInfo, threads: usize) -> Result<Box<dyn VideoDecoder>> {
        use crate::demux::Codec;
        Ok(match stream.codec {
            Codec::Av1 => Box::new(crate::decode::Av1Decoder::new(threads)?),
            Codec::Vp9 => Box::new(crate::decode::Vp9Decoder::new(threads)),
            Codec::Vp8 => Box::new(crate::decode::Vp8Decoder::new()?),
            Codec::ProRes => Box::new(crate::decode::ProResDecoder::new()),
            _ => return Err(Error::Unsupported("video codec")),
        })
    }
    fn supports_audio(&self, stream: &StreamInfo) -> bool {
        use crate::demux::Codec;
        matches!(stream.codec, Codec::Opus | Codec::Vorbis) || (cfg!(feature = "native-aac") && stream.codec == Codec::Aac)
    }
    fn open_audio_decoder(&self, stream: &StreamInfo) -> Result<Box<dyn AudioDecoder>> {
        use crate::demux::Codec;
        Ok(match stream.codec {
            Codec::Opus => Box::new(crate::decode::OpusAudioDecoder::new(stream)?),
            Codec::Vorbis => Box::new(crate::decode::VorbisAudioDecoder::new(stream)?),
            #[cfg(feature = "native-aac")]
            Codec::Aac => Box::new(crate::decode::AacAudioDecoder::new(stream)?),
            _ => return Err(Error::Unsupported("audio codec")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Codec, StreamKind};

    struct Fake(&'static str, i32);
    impl Backend for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn priority(&self) -> i32 {
            self.1
        }
        fn supports_container(&self, _: ContainerFormat) -> bool {
            false
        }
        fn open_demuxer(&self, _: ContainerFormat, _: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>> {
            unreachable!()
        }
        fn supports_video(&self, _: &StreamInfo) -> bool {
            true
        }
        fn open_video_decoder(&self, _: &StreamInfo, _: usize) -> Result<Box<dyn VideoDecoder>> {
            Err(Error::Decode("fake".into()))
        }
    }

    fn vp9() -> StreamInfo {
        StreamInfo::new(1, StreamKind::Video, Codec::Vp9)
    }

    #[test]
    fn orders_by_explicit_list_then_priority() {
        let mut r = Registry::empty();
        r.register(Arc::new(Fake("a", 0)));
        r.register(Arc::new(Fake("b", 10)));
        r.register(Arc::new(Fake("c", 5)));
        let names = |v: Vec<Arc<dyn Backend>>| v.iter().map(|b| b.name()).collect::<Vec<_>>();
        assert_eq!(names(r.ordered(None)), ["b", "c", "a"]);
        assert_eq!(names(r.ordered(Some(&["a", "zzz"]))), ["a", "b", "c"]);
    }

    #[test]
    fn unsupported_codec_reports_tried_backends() {
        let mut r = Registry::empty();
        r.register(Arc::new(Fake("fake", 0)));
        match r.open_video_decoder(&vp9(), 1, None) {
            Err(Error::UnsupportedCodec { codec, tried_backends }) => {
                assert_eq!(codec, "Vp9");
                assert_eq!(tried_backends, ["fake"]);
            }
            other => panic!("unexpected: {:?}", other.err()),
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn native_backend_does_not_claim_h264() {
        let r = Registry::with_ffmpeg(&crate::FfmpegConfig { enabled: false, ..Default::default() });
        let h264 = StreamInfo::new(1, StreamKind::Video, Codec::H264);
        assert!(matches!(
            r.open_video_decoder(&h264, 1, None),
            Err(Error::UnsupportedCodec { tried_backends, .. }) if tried_backends.is_empty()
        ));
    }

    #[cfg(feature = "native")]
    #[test]
    fn native_backend_opens_vp9() {
        assert!(Registry::with_defaults().open_video_decoder(&vp9(), 1, None).is_ok());
    }
}
