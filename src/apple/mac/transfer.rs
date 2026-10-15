//! `PixelTransfer`: VideoToolbox's pixel transfer session converting HDR pictures to SDR BGRA
//! (BT.709 primaries, transfer and matrix), scaled, on the GPU.

use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTPixelTransferSession, VTSessionSetProperty, kVTPixelTransferPropertyKey_DestinationColorPrimaries,
    kVTPixelTransferPropertyKey_DestinationTransferFunction, kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
    kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_Normal,
};

use super::cf;
use crate::{Error, Result};

pub(crate) struct PixelTransfer {
    session: CFRetained<VTPixelTransferSession>,
    /// The BGRA destination, reused while the size stays.
    destination: Option<(CFRetained<CVPixelBuffer>, (u32, u32))>,
    pool: crate::frame::FramePool,
}

// SAFETY: used only through `&mut self`, on the decode thread.
unsafe impl Send for PixelTransfer {}

impl PixelTransfer {
    pub fn new() -> Result<PixelTransfer> {
        let mut raw: *mut VTPixelTransferSession = ptr::null_mut();
        // SAFETY: the out-pointer receives a session we own.
        let session = unsafe {
            let status = VTPixelTransferSession::create(None, NonNull::from(&mut raw));
            cf::created(status, raw, "VTPixelTransferSessionCreate")?
        };
        // SAFETY: immutable framework constants.
        let settings: [(&CFString, &CFString); 4] = unsafe {
            [
                (kVTPixelTransferPropertyKey_DestinationColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2),
                (kVTPixelTransferPropertyKey_DestinationTransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2),
                (kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2),
                (kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_Normal),
            ]
        };
        for (key, value) in settings {
            // SAFETY: a live session; CFString values, as these properties take.
            let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
            if status != 0 {
                return Err(Error::Decode(format!("VTPixelTransferSession property: OSStatus {status}")));
            }
        }
        Ok(PixelTransfer { session, destination: None, pool: Default::default() })
    }

    /// `source` converted to SDR BGRA at `size` (width, height), appended to `out`.
    pub fn convert(&mut self, source: &CVPixelBuffer, size: (u32, u32)) -> Result<std::sync::Arc<[u8]>> {
        if !self.destination.as_ref().is_some_and(|(_, s)| *s == size) {
            let empty = cf::dictionary(&[]);
            // SAFETY: an immutable framework constant.
            let iosurface = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
            let attributes = cf::dictionary(&[(iosurface, empty.as_ref())]);
            let mut raw: *mut CVPixelBuffer = ptr::null_mut();
            // SAFETY: valid arguments; the out-pointer receives a buffer we own.
            let buffer = unsafe {
                let status = CVPixelBufferCreate(
                    None,
                    size.0 as usize,
                    size.1 as usize,
                    kCVPixelFormatType_32BGRA,
                    Some(&attributes),
                    NonNull::from(&mut raw),
                );
                cf::created(status, raw, "CVPixelBufferCreate")?
            };
            self.destination = Some((buffer, size));
        }
        let (destination, _) = self.destination.as_ref().expect("created above");
        // SAFETY: a live session and buffers.
        let status = unsafe { self.session.transfer_image(source, destination) };
        if status != 0 {
            return Err(Error::Decode(format!("VTPixelTransferSessionTransferImage: OSStatus {status}")));
        }
        // SAFETY: the destination is locked while read and unlocked after; rows stay within
        // `bytes per row × height`.
        unsafe {
            if CVPixelBufferLockBaseAddress(destination, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return Err(Error::Decode("could not lock the converted picture".into()));
            }
            let base = CVPixelBufferGetBaseAddress(destination) as *const u8;
            let stride = CVPixelBufferGetBytesPerRow(destination);
            let row = size.0 as usize * 4;
            if base.is_null() {
                CVPixelBufferUnlockBaseAddress(destination, CVPixelBufferLockFlags::ReadOnly);
                return Err(Error::Decode("the converted picture has no memory".into()));
            }
            let out = self.pool.fill(row * size.1 as usize, |dst| {
                for (y, line) in dst.chunks_exact_mut(row).enumerate() {
                    line.copy_from_slice(std::slice::from_raw_parts(base.add(y * stride), row));
                }
            });
            CVPixelBufferUnlockBaseAddress(destination, CVPixelBufferLockFlags::ReadOnly);
            Ok(out)
        }
    }
}

impl Drop for PixelTransfer {
    fn drop(&mut self) {
        // SAFETY: a live session, invalidated once.
        unsafe { self.session.invalidate() };
    }
}
