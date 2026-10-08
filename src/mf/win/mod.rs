//! Windows-only parts of the Media Foundation backend.

mod audio;
mod backend;
mod codecs;
mod device;
mod mft;
mod runtime;
mod video;

pub use audio::MfAudioDecoder;
pub use backend::MfBackend;
pub use video::MfVideoDecoder;
