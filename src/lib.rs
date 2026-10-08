//! Video decoding pipeline with pluggable backends. No UI dependency.

pub mod audio;
pub mod backend;
pub mod clock;
pub mod convert;
pub mod decode;
pub mod demux;
mod error;
pub mod ffmpeg;
pub mod frame;
mod player;
pub mod mf;
pub mod nal;
mod scale;
pub mod source;

pub use error::{Error, Result};
pub use ffmpeg::FfmpegConfig;
pub use frame::VideoFrame;
pub use player::{Player, PlayerConfig, PlayerEvent, PlayerState, PlayerStats, shared_thread_pool};
pub use source::Source;
