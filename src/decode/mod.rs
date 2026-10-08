//! Packets -> decoded pictures.

#[cfg(feature = "native-aac")]
mod aac;
mod audio;
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

use std::time::Duration;

#[cfg(feature = "native-aac")]
pub use aac::AacAudioDecoder;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Luma only (monochrome).
    I400,
    I420,
    I422,
    I444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl ColorMatrix {
    /// From an ITU-T H.273 MatrixCoefficients value (as in containers and codec headers).
    pub fn from_h273(m: u8) -> Option<ColorMatrix> {
        match m {
            1 => Some(ColorMatrix::Bt709),
            5 | 6 => Some(ColorMatrix::Bt601),
            9 | 10 => Some(ColorMatrix::Bt2020),
            _ => None,
        }
    }

    /// Fallback when the stream does not say: HD and larger is BT.709, SD is BT.601.
    pub fn guess_for_height(height: u32) -> ColorMatrix {
        if height >= 720 { ColorMatrix::Bt709 } else { ColorMatrix::Bt601 }
    }
}

/// A decoded 8-bit planar YUV picture. Higher bit depths are shifted down to 8 bits by the decoder.
#[derive(Clone, Debug)]
pub struct YuvFrame {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    /// Y, U, V. U and V are empty for `I400`.
    pub planes: [Vec<u8>; 3],
    /// Bytes per row for each plane.
    pub strides: [usize; 3],
    pub matrix: ColorMatrix,
    pub full_range: bool,
    pub pts: Duration,
}

impl YuvFrame {
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
    // Phase 4: Platform(PlatformSurface), e.g. a CVPixelBuffer from VideoToolbox.
}

impl DecodedFrame {
    pub fn pts(&self) -> Duration {
        match self {
            DecodedFrame::Yuv(f) => f.pts,
        }
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
}
