//! macOS-only parts: VideoToolbox and AudioToolbox through the objc2 framework bindings.

mod audio;
mod backend;
mod cf;
mod video;

pub use audio::AtAudioDecoder;
pub use backend::AppleBackend;
pub use video::VtVideoDecoder;
