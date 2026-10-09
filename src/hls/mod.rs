//! HTTP Live Streaming: playlists, segment fetching, and a demuxer that plays them.
// TEMP until the demuxer uses everything (removed in the HlsDemuxer task).
#![allow(dead_code)]

pub mod playlist;
pub(crate) mod abr;
pub(crate) mod crypto;
pub(crate) mod segment;
