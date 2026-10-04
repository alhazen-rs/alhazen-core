//! Video decoding pipeline with pluggable backends. No UI dependency.

pub mod decode;
pub mod demux;
mod error;
pub mod source;

pub use error::{Error, Result};
pub use source::Source;
