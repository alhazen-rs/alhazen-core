//! `PixelTransfer`: VideoToolbox's pixel transfer session converting pictures to BGRA, scaled,
//! on the GPU: SDR as it is (like the CPU path), HDR tone mapped to BT.709.

use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimariesKey,
    kCVImageBufferColorPrimaries_EBU_3213, kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTPixelTransferSession, VTSessionSetProperty, kVTPixelTransferPropertyKey_DestinationColorPrimaries,
    kVTPixelTransferPropertyKey_DestinationTransferFunction, kVTPixelTransferPropertyKey_ScalingMode, kVTScalingMode_Normal,
};

use crate::color::{ColorInfo, ColorMatrix, Primaries, Transfer};

use super::cf;
use crate::{Error, Result};

pub(crate) struct PixelTransfer {
    session: CFRetained<VTPixelTransferSession>,
    /// The BGRA destination, reused while the size stays.
    destination: Option<(CFRetained<CVPixelBuffer>, (u32, u32))>,
    pool: crate::frame::FramePool,
    /// The destination colour space the session is set to.
    target: Option<(Primaries, bool)>,
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
        let transfer = PixelTransfer { session, destination: None, pool: Default::default(), target: None };
        // SAFETY: immutable framework constants.
        transfer.set(unsafe { kVTPixelTransferPropertyKey_ScalingMode }, unsafe { kVTScalingMode_Normal })?;
        Ok(transfer)
    }

    fn set(&self, key: &CFString, value: &CFString) -> Result<()> {
        // SAFETY: a live session; CFString values, as these properties take.
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            return Err(Error::Decode(format!("VTPixelTransferSession property: OSStatus {status}")));
        }
        Ok(())
    }

    /// `source` (described by `color`) converted to SDR BGRA at `size` (width, height). SDR keeps
    /// its primaries and transfer, as the CPU path does; HDR is tone mapped to BT.709.
    pub fn convert(&mut self, source: &CVPixelBuffer, size: (u32, u32), color: &ColorInfo) -> Result<std::sync::Arc<[u8]>> {
        // The source as we read it (not what VideoToolbox would guess for an untagged stream).
        // SAFETY: immutable framework constants; a live buffer we hold a reference to.
        unsafe {
            let matrix = match color.matrix {
                ColorMatrix::Bt601 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                ColorMatrix::Bt709 => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ColorMatrix::Bt2020 => kCVImageBufferYCbCrMatrix_ITU_R_2020,
            };
            let transfer = match color.transfer {
                Transfer::Sdr => kCVImageBufferTransferFunction_ITU_R_709_2,
                Transfer::Pq => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                Transfer::Hlg => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            };
            let mode = CVAttachmentMode::ShouldPropagate;
            source.set_attachment(kCVImageBufferYCbCrMatrixKey, matrix, mode);
            source.set_attachment(kCVImageBufferColorPrimariesKey, primaries_key(color.primaries), mode);
            source.set_attachment(kCVImageBufferTransferFunctionKey, transfer, mode);
        }
        let hdr = color.transfer.is_hdr();
        let target = (if hdr { Primaries::Bt709 } else { color.primaries }, hdr);
        if self.target != Some(target) {
            // SAFETY: immutable framework constants.
            unsafe {
                self.set(kVTPixelTransferPropertyKey_DestinationColorPrimaries, primaries_key(target.0))?;
                self.set(kVTPixelTransferPropertyKey_DestinationTransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2)?;
            }
            self.target = Some(target);
        }
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

/// CoreVideo's name for these primaries.
///
/// # Safety
/// Reads immutable framework constants.
unsafe fn primaries_key(p: Primaries) -> &'static CFString {
    // SAFETY: as documented.
    unsafe {
        match p {
            Primaries::Bt601 => kCVImageBufferColorPrimaries_EBU_3213,
            Primaries::Bt709 => kCVImageBufferColorPrimaries_ITU_R_709_2,
            Primaries::Bt2020 => kCVImageBufferColorPrimaries_ITU_R_2020,
        }
    }
}

impl Drop for PixelTransfer {
    fn drop(&mut self) {
        // SAFETY: a live session, invalidated once.
        unsafe { self.session.invalidate() };
    }
}
