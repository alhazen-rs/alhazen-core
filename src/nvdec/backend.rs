//! `NvdecBackend`: claims video streams the GPU decodes, by the shared hardware rules.

use crate::backend::Backend;
use crate::decode::VideoDecoder;
use crate::demux::{ContainerFormat, Demuxer, StreamInfo};
use crate::hw::select::{self, Catalogue, HwCodec};
use crate::source::MediaSource;
use crate::{Error, Result};

use super::decoder::NvdecVideoDecoder;
use super::device;

/// What the GPU decodes (8-bit 4:2:0 suffices to claim; a 10-bit stream the GPU lacks fails at
/// the first packet and falls back to the next backend).
struct Gpu;

impl Catalogue for Gpu {
    fn has_decoder(&self, codec: HwCodec, _hardware_only: bool) -> bool {
        let Some(codec) = device::cuda_codec(codec) else { return false };
        device::get().is_some_and(|d| d.caps(codec, 8).is_some())
    }
}

/// Whether the stream's size is within what the GPU decodes (an unknown size, 0×0, passes: the
/// decoder then reports a size it can't handle as an error, and playback falls back).
fn fits(stream: &StreamInfo) -> bool {
    let (w, h) = (stream.width, stream.height);
    if w == 0 || h == 0 {
        return true;
    }
    let caps = HwCodec::of(stream)
        .and_then(device::cuda_codec)
        .and_then(|c| device::get().and_then(|d| d.caps(c, 8)));
    caps.is_some_and(|c| w >= c.min.0 && h >= c.min.1 && w <= c.max.0 && h <= c.max.1)
}

/// NVIDIA's GPU decoders on Linux.
pub struct NvdecBackend {
    prefer_hardware: bool,
}

impl NvdecBackend {
    pub fn new(prefer_hardware: bool) -> Self {
        Self { prefer_hardware }
    }
}

impl Backend for NvdecBackend {
    fn name(&self) -> &'static str {
        "nvdec"
    }
    fn priority(&self) -> i32 {
        select::priority(self.prefer_hardware)
    }
    fn supports_container(&self, _: ContainerFormat) -> bool {
        false
    }
    fn open_demuxer(&self, _: ContainerFormat, _: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>> {
        Err(Error::UnsupportedContainer)
    }
    fn supports_video(&self, stream: &StreamInfo) -> bool {
        select::claims(stream, &Gpu, self.prefer_hardware) && fits(stream)
    }
    fn open_video_decoder(&self, stream: &StreamInfo, _threads: usize) -> Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(NvdecVideoDecoder::new(stream)?))
    }
}
