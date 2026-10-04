//! Video decoding pipeline with pluggable backends. No UI dependency.

mod error;
pub mod source;

pub use error::{Error, Result};
pub use source::Source;
