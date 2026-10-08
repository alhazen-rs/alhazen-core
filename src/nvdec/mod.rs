//! The NVDEC backend: NVIDIA's GPU decoders on Linux, behind our demuxers. Loaded at runtime
//! from the driver's `libcuda.so.1` and `libnvcuvid.so.1`; without them the backend is absent.

pub mod size;
#[cfg(all(target_os = "linux", feature = "nvdec"))]
mod sys;

/// Whether NVDEC can be used on this machine (driver libraries present, a CUDA device exists).
pub fn available() -> bool {
    #[cfg(all(target_os = "linux", feature = "nvdec"))]
    return sys::Api::get().is_some();
    #[cfg(not(all(target_os = "linux", feature = "nvdec")))]
    false
}
