//! The VA-API backend: AMD's and Intel's GPU decoders on Linux, behind our demuxers. libva is
//! loaded at runtime (`libva.so.2`, `libva-drm.so.2`); without it, or without an AMD or Intel
//! GPU, the backend is absent. NVIDIA GPUs are left to NVDEC, which does more
//! (`ALHAZEN_VAAPI_ANY_DRIVER=1` takes any driver, to test this backend anywhere).
//!
//! VA-API decodes slices and nothing else: reading the stream's headers, picture order,
//! references and output order are ours ([`h264`]). H.264 so far (progressive, 8-bit 4:2:0).

pub mod h264;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod backend;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod decoder;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod display;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
mod sys;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub use backend::VaapiBackend;
#[cfg(all(target_os = "linux", feature = "vaapi"))]
pub use decoder::VaapiVideoDecoder;

/// Whether an AMD or Intel GPU decodes through VA-API here (libva present, a render node
/// with such a driver).
pub fn available() -> bool {
    #[cfg(all(target_os = "linux", feature = "vaapi"))]
    return display::caps().is_some();
    #[cfg(not(all(target_os = "linux", feature = "vaapi")))]
    false
}
