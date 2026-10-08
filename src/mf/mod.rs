//! The Media Foundation backend: Windows' own decoders (GPU-accelerated where the hardware
//! supports the codec; patent licensing covered by Windows) behind our demuxers.
//!
//! `select` and `setup` are platform-independent (tested everywhere); the rest is Windows-only.

/// Kept as `mf::select` for existing users; the rules are shared with NVDEC.
pub use crate::hw::select;
pub mod setup;

#[cfg(all(windows, feature = "media-foundation"))]
mod win;
#[cfg(all(windows, feature = "media-foundation"))]
pub use win::{MfAudioDecoder, MfBackend, MfVideoDecoder};
