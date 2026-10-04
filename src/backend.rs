//! Backend registry: which demuxers/decoders exist, and in which order to try them.

use std::sync::Arc;

use crate::decode::VideoDecoder;
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
}

#[derive(Clone, Default)]
pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
}

impl Registry {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every backend compiled in through Cargo features.
    pub fn with_defaults() -> Self {
        #[cfg_attr(not(feature = "native"), allow(unused_mut))]
        let mut r = Self::empty();
        #[cfg(feature = "native")]
        r.register(Arc::new(NativeBackend));
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

    /// Opens a video decoder for `stream`, trying each capable backend in order.
    pub fn open_video_decoder(
        &self,
        stream: &StreamInfo,
        threads: usize,
        order: Option<&[&'static str]>,
    ) -> Result<Box<dyn VideoDecoder>> {
        let mut tried = Vec::new();
        for b in self.ordered(order).into_iter().filter(|b| b.supports_video(stream)) {
            tried.push(b.name());
            match b.open_video_decoder(stream, threads) {
                Ok(d) => return Ok(d),
                Err(e) => log::warn!("backend {} failed to open {} decoder: {e}", b.name(), stream.codec),
            }
        }
        Err(Error::UnsupportedCodec { codec: stream.codec.to_string(), tried_backends: tried })
    }
}

/// Pure-Rust backend: Matroska/WebM + MP4 demuxing, AV1 decoding via rav1d.
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
        stream.codec == crate::demux::Codec::Av1
    }
    fn open_video_decoder(&self, _stream: &StreamInfo, threads: usize) -> Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(crate::decode::Av1Decoder::new(threads)?))
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
        StreamInfo {
            id: 1,
            kind: StreamKind::Video,
            codec: Codec::Vp9,
            width: 0,
            height: 0,
            duration: None,
            extradata: None,
        }
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
    fn native_backend_does_not_claim_vp9() {
        let r = Registry::with_defaults();
        assert!(matches!(
            r.open_video_decoder(&vp9(), 1, None),
            Err(Error::UnsupportedCodec { tried_backends, .. }) if tried_backends.is_empty()
        ));
    }
}
