//! `VaapiBackend`: claims the video streams an AMD or Intel GPU decodes, by the shared
//! hardware rules.

use crate::backend::Backend;
use crate::decode::VideoDecoder;
use crate::demux::{ContainerFormat, Demuxer, StreamInfo};
use crate::hw::select::{self, Catalogue, HwCodec};
use crate::nvdec::profile::{self, Chroma, Format};
use crate::source::MediaSource;
use crate::{Error, Result};

use super::decoder::VaapiVideoDecoder;
use super::display::{caps, h264_profile};

struct Gpu;

impl Catalogue for Gpu {
    fn has_decoder(&self, codec: HwCodec, _hardware_only: bool) -> bool {
        // H.264 so far; HEVC, VP9 and AV1 come next.
        codec == HwCodec::H264 && caps().is_some_and(|c| h264_profile(100, 0, &c.profiles).is_some() || h264_profile(77, 0, &c.profiles).is_some())
    }
}

/// 8-bit 4:2:0 in a profile the GPU has (as far as the setup data tells; anything missed fails
/// at the first keyframe and the next backend takes over).
fn fits(stream: &StreamInfo) -> bool {
    let format = profile::format(stream).unwrap_or(Format { bit_depth: 8, chroma: Chroma::Yuv420 });
    if format.chroma != Chroma::Yuv420 || format.bit_depth != 8 {
        return false;
    }
    // avcC's second byte is the profile.
    let profile_idc = stream.extradata.as_deref().and_then(|c| c.get(1)).map_or(100, |p| u32::from(*p));
    caps().is_some_and(|c| h264_profile(profile_idc, 0, &c.profiles).is_some())
}

/// AMD's and Intel's GPU decoders on Linux.
pub struct VaapiBackend {
    prefer_hardware: bool,
}

impl VaapiBackend {
    pub fn new(prefer_hardware: bool) -> Self {
        Self { prefer_hardware }
    }
}

impl Backend for VaapiBackend {
    fn name(&self) -> &'static str {
        "vaapi"
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
    fn open_video_decoder(&self, stream: &StreamInfo, _threads: usize, _prefs: crate::output::OutputPrefs) -> Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(VaapiVideoDecoder::new(stream)?))
    }
}
