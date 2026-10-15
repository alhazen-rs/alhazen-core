//! Packets -> decoded pictures.

#[cfg(feature = "native-aac")]
mod aac;
mod audio;
#[cfg(feature = "native-mp3")]
mod mp3;
#[cfg(feature = "native")]
mod channels;
#[cfg(feature = "native")]
mod av1;
#[cfg(feature = "native")]
mod opus;
#[cfg(feature = "native")]
mod pcm;
#[cfg(feature = "native")]
mod planar;
#[cfg(feature = "native")]
mod prores;
#[cfg(feature = "native")]
mod opus_multistream;
#[cfg(feature = "native")]
mod flac;
#[cfg(feature = "native")]
mod vorbis;
#[cfg(feature = "native")]
mod vp8;
#[cfg(feature = "native")]
mod vp9;

#[cfg(any(feature = "native", feature = "native-aac", feature = "native-mp3", feature = "ffmpeg-cli", all(windows, feature = "media-foundation"), all(target_os = "macos", feature = "videotoolbox")))]
mod delay;
#[cfg(any(feature = "native-aac", feature = "native-mp3", feature = "ffmpeg-cli", all(windows, feature = "media-foundation"), all(target_os = "macos", feature = "videotoolbox")))]
pub(crate) use delay::DelayTrim;
#[cfg(feature = "native")]
pub(crate) use delay::clip_end;

use std::time::Duration;

#[cfg(feature = "native-aac")]
pub use aac::AacAudioDecoder;
#[cfg(feature = "native-mp3")]
pub use mp3::Mp3AudioDecoder;
pub use audio::{AudioBuffer, AudioDecoder};
#[cfg(feature = "native")]
pub use av1::Av1Decoder;
#[cfg(feature = "native")]
pub use opus::OpusAudioDecoder;
#[cfg(feature = "native")]
pub use flac::FlacAudioDecoder;
#[cfg(feature = "native")]
pub use vorbis::VorbisAudioDecoder;
#[cfg(feature = "native")]
pub use pcm::PcmAudioDecoder;
#[cfg(feature = "native")]
pub use prores::ProResDecoder;
#[cfg(feature = "native")]
pub use vp8::Vp8Decoder;
#[cfg(feature = "native")]
pub use vp9::Vp9Decoder;

use crate::Result;
use crate::demux::Packet;
pub use crate::color::{ColorInfo, ColorMatrix};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Luma only (monochrome).
    I400,
    I420,
    I422,
    I444,
}

/// The samples of a planar YUV picture: 8-bit, or 16-bit holding up to 16 significant bits,
/// LSB-aligned (0–1023 for 10-bit).
#[derive(Clone, Debug)]
pub enum YuvPlanes {
    U8([Vec<u8>; 3]),
    U16([Vec<u16>; 3]),
}

/// A decoded planar YUV picture. 8-bit unless 10-bit output was asked for (`FrameFormat::Yuv10`).
#[derive(Clone, Debug)]
pub struct YuvFrame {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    /// Y, U, V. U and V are empty for `I400`.
    pub planes: YuvPlanes,
    /// Significant bits per sample: 8 for `U8` planes, 10 for 10-bit `U16` planes.
    pub bit_depth: u8,
    /// Samples per row for each plane.
    pub strides: [usize; 3],
    pub color: ColorInfo,
    pub pts: Duration,
}

impl YuvFrame {
    pub fn u8_planes(&self) -> Option<&[Vec<u8>; 3]> {
        match &self.planes {
            YuvPlanes::U8(p) => Some(p),
            YuvPlanes::U16(_) => None,
        }
    }

    pub fn u16_planes(&self) -> Option<&[Vec<u16>; 3]> {
        match &self.planes {
            YuvPlanes::U16(p) => Some(p),
            YuvPlanes::U8(_) => None,
        }
    }

    /// Chroma plane dimensions for this layout.
    pub fn chroma_size(&self) -> (u32, u32) {
        chroma_size(self.layout, self.width, self.height)
    }
}

pub fn chroma_size(layout: PixelLayout, width: u32, height: u32) -> (u32, u32) {
    match layout {
        PixelLayout::I400 => (0, 0),
        PixelLayout::I420 => (width.div_ceil(2), height.div_ceil(2)),
        PixelLayout::I422 => (width.div_ceil(2), height),
        PixelLayout::I444 => (width, height),
    }
}

#[derive(Clone, Debug)]
pub enum DecodedFrame {
    Yuv(YuvFrame),
    /// Already converted on the GPU: tightly packed SDR BGRA (tone mapped when the source is HDR),
    /// at the size asked for with `set_output_hint`. `transfer` is the source's.
    Bgra { width: u32, height: u32, bgra: Vec<u8>, transfer: crate::color::Transfer, pts: Duration },
    /// macOS: VideoToolbox's own pixel buffer, kept for zero-copy drawing (SDR only).
    #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
    Native(crate::apple::NativeFrame),
}

impl DecodedFrame {
    pub fn pts(&self) -> Duration {
        match self {
            DecodedFrame::Yuv(f) => f.pts,
            DecodedFrame::Bgra { pts, .. } => *pts,
            #[cfg(all(target_os = "macos", feature = "videotoolbox"))]
            DecodedFrame::Native(f) => f.pts,
        }
    }
}

impl From<YuvFrame> for DecodedFrame {
    fn from(f: YuvFrame) -> DecodedFrame {
        DecodedFrame::Yuv(f)
    }
}

pub trait VideoDecoder: Send {
    fn send_packet(&mut self, packet: &Packet) -> Result<()>;
    /// `Ok(None)` means the decoder needs more input.
    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>>;
    /// Drops all buffered state; called on seek.
    fn flush(&mut self);
    /// No more packets until the next `flush`: decoders with delayed output (an external
    /// process) make `receive_frame` wait for and return the rest, then `Ok(None)`.
    fn send_eof(&mut self) {}
    /// The largest frame size wanted (usually the display size in device pixels), so decoders
    /// that can scale cheaply (NVDEC's hardware scaler) produce frames no larger. Frames may
    /// still come out larger; the pipeline scales those down. Called before every packet.
    fn set_output_hint(&mut self, _max: Option<(u32, u32)>) {}
    /// Converting on the GPU was asked for (`OutputPrefs::gpu_bgra`) and has failed: the decoder
    /// now hands out YUV for the CPU path.
    fn gpu_fallback(&self) -> bool {
        false
    }
}
