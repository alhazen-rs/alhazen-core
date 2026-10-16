//! `NativeFrame`: a picture VideoToolbox decoded, kept in its own (GPU-shareable) pixel buffer
//! so a renderer can draw it without copying it.

use std::ffi::c_void;
use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;

use crate::color::ColorInfo;
use crate::decode::YuvFrame;
use crate::{Error, Result};

/// A decoded picture in VideoToolbox's pixel buffer (IOSurface-backed, Metal-compatible),
/// retained for as long as this frame lives.
#[derive(Clone)]
pub struct NativeFrame {
    pixel_buffer: CFRetained<CVPixelBuffer>,
    pub width: u32,
    pub height: u32,
    pub color: ColorInfo,
    pub pts: Duration,
}

// SAFETY: retaining and releasing a CoreVideo buffer is thread-safe, and VideoToolbox does not
// write to a picture once it has handed it out: the buffer is only read from here on.
unsafe impl Send for NativeFrame {}
// SAFETY: as above; every method takes `&self` and only reads.
unsafe impl Sync for NativeFrame {}

impl NativeFrame {
    pub(crate) fn new(pixel_buffer: CFRetained<CVPixelBuffer>, width: u32, height: u32, color: ColorInfo, pts: Duration) -> NativeFrame {
        NativeFrame { pixel_buffer, width, height, color, pts }
    }

    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.pixel_buffer
    }

    /// The `CVPixelBufferRef`, for APIs that take the raw pointer (e.g. the `core-video` crate's
    /// `CVPixelBuffer::wrap_under_get_rule`). Valid while this frame lives.
    pub fn as_ptr(&self) -> *mut c_void {
        CFRetained::as_ptr(&self.pixel_buffer).as_ptr().cast()
    }

    /// The picture copied out as planar YUV (8-bit), for consumers that can't draw it natively.
    pub fn to_yuv(&self) -> Result<YuvFrame> {
        // SAFETY: a live pixel buffer.
        unsafe { super::video::copy_picture(&self.pixel_buffer, self.pts, Some(self.color.matrix), self.height, false) }
            .map(|mut f| {
                f.color = self.color;
                f
            })
            .map_err(Error::Decode)
    }
}

impl std::fmt::Debug for NativeFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFrame").field("size", &(self.width, self.height)).field("color", &self.color).field("pts", &self.pts).finish()
    }
}
