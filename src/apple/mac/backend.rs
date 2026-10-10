//! `AppleBackend`: claims what VideoToolbox and AudioToolbox decode on this Mac.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use objc2_video_toolbox::{VTIsHardwareDecodeSupported, VTRegisterSupplementalVideoDecoderIfAvailable};

use super::{AtAudioDecoder, VtVideoDecoder};
use crate::apple::{format::video_codec_type, rules};
use crate::backend::Backend;
use crate::decode::{AudioDecoder, VideoDecoder};
use crate::demux::{Codec, ContainerFormat, Demuxer, StreamInfo};
use crate::hw::select::{self, Catalogue, HwCodec};
use crate::source::MediaSource;
use crate::{Error, Result};

/// Whether the Mac decodes `codec` in hardware (asked once per codec).
fn hardware(codec: HwCodec) -> bool {
    static FOUND: OnceLock<Mutex<HashMap<HwCodec, bool>>> = OnceLock::new();
    *FOUND.get_or_init(Default::default).lock().unwrap().entry(codec).or_insert_with(|| {
        let codec_type = match codec {
            HwCodec::ProRes => u32::from_be_bytes(*b"apcn"),
            HwCodec::Av1 => video_codec_type(&Codec::Av1).unwrap(),
            HwCodec::Vp9 => video_codec_type(&Codec::Vp9).unwrap(),
            HwCodec::H264 => video_codec_type(&Codec::H264).unwrap(),
            HwCodec::Hevc => video_codec_type(&Codec::Hevc).unwrap(),
            _ => return false,
        };
        // SAFETY: plain queries; VP9's and AV1's decoders are supplemental ones that must be
        // registered first.
        unsafe {
            if matches!(codec, HwCodec::Vp9 | HwCodec::Av1) {
                VTRegisterSupplementalVideoDecoderIfAvailable(codec_type);
            }
            VTIsHardwareDecodeSupported(codec_type)
        }
    })
}

struct Apple;

impl Catalogue for Apple {
    fn has_decoder(&self, codec: HwCodec, hardware_only: bool) -> bool {
        rules::has_decoder(codec, hardware_only, hardware)
    }
}

/// Apple's decoders: VideoToolbox (video) and AudioToolbox (audio).
pub struct AppleBackend {
    prefer_hardware: bool,
}

impl AppleBackend {
    pub fn new(prefer_hardware: bool) -> Self {
        Self { prefer_hardware }
    }
}

impl Backend for AppleBackend {
    fn name(&self) -> &'static str {
        "videotoolbox"
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
        select::claims(stream, &Apple, self.prefer_hardware)
    }
    fn open_video_decoder(&self, stream: &StreamInfo, _threads: usize) -> Result<Box<dyn VideoDecoder>> {
        Ok(Box::new(VtVideoDecoder::new(stream)?))
    }
    fn supports_audio(&self, stream: &StreamInfo) -> bool {
        // AAC only when it is USAC; the native decoder takes the rest.
        let usac_or_other = stream.codec != Codec::Aac || stream.extradata.as_deref().is_some_and(rules::is_usac);
        usac_or_other && select::claims(stream, &Apple, self.prefer_hardware)
    }
    fn open_audio_decoder(&self, stream: &StreamInfo) -> Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(AtAudioDecoder::new(stream)?))
    }
}
