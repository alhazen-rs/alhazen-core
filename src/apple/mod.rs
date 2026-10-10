//! Apple's decoders on macOS: VideoToolbox (video) and AudioToolbox (audio).
//!
//! The rules, format and plane helpers are platform-independent (tested everywhere); the
//! decoders themselves are macOS-only.

// Off macOS only the tests use the helpers.
#![cfg_attr(not(all(target_os = "macos", feature = "videotoolbox")), allow(dead_code))]

pub(crate) mod rules;
pub(crate) mod format;
pub(crate) mod planes;
