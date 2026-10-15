//! Video decoding pipeline with pluggable backends. No UI dependency.

pub mod audio;
pub mod apple;
pub mod backend;
pub mod clock;
pub mod convert;
pub mod decode;
pub mod demux;
mod error;
pub mod ffmpeg;
pub mod frame;
pub mod hw;
#[cfg(feature = "hls")]
pub mod hls;
mod player;
pub mod mf;
pub mod nal;
pub mod nvdec;
mod scale;
pub mod source;

pub use demux::{Metadata, Picture};
pub use error::{Error, Result};
pub use ffmpeg::FfmpegConfig;
pub use frame::VideoFrame;
pub use player::{Player, PlayerConfig, PlayerEvent, PlayerState, PlayerStats, shared_thread_pool};
pub use source::Source;
