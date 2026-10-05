//! Video decoding pipeline with pluggable backends. No UI dependency.

pub mod audio;
pub mod backend;
pub mod clock;
pub mod convert;
pub mod decode;
pub mod demux;
mod error;
pub mod frame;
mod player;
pub mod source;

pub use error::{Error, Result};
pub use frame::VideoFrame;
pub use player::{Player, PlayerConfig, PlayerEvent, PlayerState, shared_thread_pool};
pub use source::Source;
