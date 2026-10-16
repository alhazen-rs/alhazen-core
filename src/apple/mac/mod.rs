//! macOS-only parts: VideoToolbox and AudioToolbox through the objc2 framework bindings.

mod audio;
mod backend;
mod cf;
mod native;
mod transfer;
mod video;

pub use audio::AtAudioDecoder;
pub use backend::AppleBackend;
pub use native::NativeFrame;
pub use video::VtVideoDecoder;
