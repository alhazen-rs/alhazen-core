//! One CUDA context for the process, shared by every NVDEC decoder, and what the GPU decodes.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::sys::*;
use crate::hw::select::HwCodec;
use crate::{Error, Result};

/// The CUDA context NVDEC works in (device 0), created on first use and never destroyed.
pub struct Device {
    api: &'static Api,
    ctx: CUcontext,
    caps: Mutex<HashMap<(cudaVideoCodec, u32), Option<Caps>>>,
}

// SAFETY: a CUDA context may be made current on any thread; we push and pop it around each use
// (`Device::push`), and `caps` is behind a mutex.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// What NVDEC supports for one codec and bit depth (4:2:0).
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub max: (u32, u32),
    pub min: (u32, u32),
}

/// The process's NVDEC device, or `None` without NVIDIA's driver or a CUDA device.
pub fn get() -> Option<&'static Device> {
    static DEVICE: OnceLock<Option<Device>> = OnceLock::new();
    DEVICE.get_or_init(Device::open).as_ref()
}

/// NVDEC's id for a video codec we decode with it.
pub fn cuda_codec(codec: HwCodec) -> Option<cudaVideoCodec> {
    Some(match codec {
        HwCodec::H264 => CODEC_H264,
        HwCodec::Hevc => CODEC_HEVC,
        HwCodec::Vp8 => CODEC_VP8,
        HwCodec::Vp9 => CODEC_VP9,
        HwCodec::Av1 => CODEC_AV1,
        _ => return None,
    })
}

/// A CUDA/NVDEC status as our error.
pub fn check(r: CUresult, what: &str) -> Result<()> {
    if r == CUDA_SUCCESS { Ok(()) } else { Err(Error::Decode(format!("NVDEC {what}: CUDA error {r}"))) }
}

impl Device {
    fn open() -> Option<Device> {
        let api = Api::get()?;
        // SAFETY: plain driver calls with valid out-pointers.
        unsafe {
            if (api.cuInit)(0) != CUDA_SUCCESS {
                return None;
            }
            let mut dev = 0;
            if (api.cuDeviceGet)(&mut dev, 0) != CUDA_SUCCESS {
                return None;
            }
            let mut ctx = std::ptr::null_mut();
            if (api.cuCtxCreate)(&mut ctx, 0, dev) != CUDA_SUCCESS {
                return None;
            }
            // Created contexts start current on this thread; users push it themselves.
            let mut popped = std::ptr::null_mut();
            (api.cuCtxPopCurrent)(&mut popped);
            Some(Device { api, ctx, caps: Mutex::default() })
        }
    }

    pub fn api(&self) -> &'static Api {
        self.api
    }

    /// Makes the context current on this thread until the guard drops.
    pub fn push(&self) -> Result<Current<'_>> {
        // SAFETY: `ctx` is a live context (never destroyed).
        check(unsafe { (self.api.cuCtxPushCurrent)(self.ctx) }, "context")?;
        Ok(Current(self))
    }

    /// Whether (and up to what size) the GPU decodes `codec` at `bit_depth`, 4:2:0. Cached.
    pub fn caps(&self, codec: cudaVideoCodec, bit_depth: u32) -> Option<Caps> {
        if let Some(c) = self.caps.lock().unwrap().get(&(codec, bit_depth)) {
            return *c;
        }
        let found = self.query(codec, bit_depth);
        self.caps.lock().unwrap().insert((codec, bit_depth), found);
        found
    }

    fn query(&self, codec: cudaVideoCodec, bit_depth: u32) -> Option<Caps> {
        let _current = self.push().ok()?;
        let mut c: CUVIDDECODECAPS = zeroed();
        c.eCodecType = codec;
        c.eChromaFormat = CHROMA_420;
        c.nBitDepthMinus8 = bit_depth.checked_sub(8)?;
        // SAFETY: `c` is a valid in/out struct; the context is current.
        let ok = unsafe { (self.api.cuvidGetDecoderCaps)(&mut c) } == CUDA_SUCCESS;
        (ok && c.bIsSupported != 0)
            .then_some(Caps { max: (c.nMaxWidth, c.nMaxHeight), min: (c.nMinWidth as u32, c.nMinHeight as u32) })
    }
}

/// The context is current on this thread while this lives.
pub struct Current<'a>(&'a Device);

impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut popped = std::ptr::null_mut();
        // SAFETY: balances the push in `Device::push` on this thread.
        unsafe { (self.0.api.cuCtxPopCurrent)(&mut popped) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::select::HwCodec;

    #[test]
    fn maps_only_video_codecs() {
        assert_eq!(cuda_codec(HwCodec::H264), Some(CODEC_H264));
        assert_eq!(cuda_codec(HwCodec::Av1), Some(CODEC_AV1));
        assert_eq!(cuda_codec(HwCodec::Aac), None);
    }

    /// On this machine (RTX 4060) every codec decodes at 8 bits, HEVC/VP9/AV1 at 10. Elsewhere
    /// the test only checks that asking never fails loudly.
    #[test]
    fn reports_the_gpus_decoders() {
        let Some(dev) = get() else {
            eprintln!("skipped: no NVDEC");
            return;
        };
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_VP8, CODEC_VP9, CODEC_AV1] {
            let caps = dev.caps(codec, 8);
            eprintln!("codec {codec} 8-bit: {caps:?}");
            assert!(caps.is_some_and(|c| c.max.0 >= 1920 && c.max.1 >= 1080), "codec {codec}");
        }
        for codec in [CODEC_HEVC, CODEC_VP9, CODEC_AV1] {
            assert!(dev.caps(codec, 10).is_some(), "codec {codec} 10-bit");
        }
        assert!(dev.caps(CODEC_H264, 12).is_none(), "no 12-bit H.264");
    }

    #[test]
    fn the_context_can_be_pushed_from_any_thread() {
        let Some(dev) = get() else { return };
        let handles: Vec<_> = (0..4).map(|_| std::thread::spawn(move || dev.push().map(|_| ()).is_ok())).collect();
        assert!(handles.into_iter().all(|h| h.join().unwrap()));
    }
}
