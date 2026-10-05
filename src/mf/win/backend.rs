//! `MfBackend`: claims what Media Foundation can decode on this machine.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::super::select::{self, Catalogue, MfCodec};
use super::{codecs, device, mft, runtime};
use crate::backend::Backend;
use crate::decode::{AudioDecoder, VideoDecoder};
use crate::demux::{ContainerFormat, Demuxer, StreamInfo};
use crate::source::MediaSource;
use crate::{Error, Result};

/// Installed decoders, asked once per codec for the process.
struct Installed;

impl Catalogue for Installed {
    fn has_decoder(&self, codec: MfCodec, hardware_only: bool) -> bool {
        static FOUND: OnceLock<Mutex<HashMap<MfCodec, bool>>> = OnceLock::new();
        let found = *FOUND.get_or_init(Default::default).lock().unwrap().entry(codec).or_insert_with(|| {
            runtime::com_init();
            if runtime::ensure_started().is_err() {
                return false;
            }
            let (category, major, subtype) = codecs::ids(codec);
            mft::find_decoder(category, major, subtype).is_some()
        });
        found && (!hardware_only || device::gpu().is_some_and(|g| g.decodes(codec)))
    }
}

/// Windows' own decoders (Media Foundation), GPU-accelerated where possible.
pub struct MfBackend {
    prefer_hardware: bool,
}

impl MfBackend {
    pub fn new(prefer_hardware: bool) -> Self {
        Self { prefer_hardware }
    }
}

impl Backend for MfBackend {
    fn name(&self) -> &'static str {
        "media-foundation"
    }
    fn priority(&self) -> i32 {
        select::priority(self.prefer_hardware)
    }
    fn supports_container(&self, _: ContainerFormat) -> bool {
        false
    }
    fn open_demuxer(&self, _: ContainerFormat, _: Box<dyn MediaSource>) -> Result<Box<dyn Demuxer>> {
        Err(Error::Unsupported("Media Foundation demuxing"))
    }
    fn supports_video(&self, stream: &StreamInfo) -> bool {
        select::claims(stream, &Installed, self.prefer_hardware)
    }
    fn open_video_decoder(&self, stream: &StreamInfo, _threads: usize) -> Result<Box<dyn VideoDecoder>> {
        let codec = MfCodec::of(stream).ok_or(Error::Unsupported("codec"))?;
        Ok(Box::new(super::MfVideoDecoder::new(codec, stream, true)?))
    }
    fn supports_audio(&self, stream: &StreamInfo) -> bool {
        select::claims(stream, &Installed, self.prefer_hardware)
    }
    fn open_audio_decoder(&self, stream: &StreamInfo) -> Result<Box<dyn AudioDecoder>> {
        let codec = MfCodec::of(stream).ok_or(Error::Unsupported("codec"))?;
        Ok(Box::new(super::MfAudioDecoder::new(codec, stream)?))
    }
}
