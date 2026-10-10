//! HTTP Live Streaming: playlists, segment fetching, and a demuxer that plays them.

pub mod playlist;
mod demuxer;
pub(crate) mod timeline;

pub use demuxer::{AudioRendition, HlsControl, HlsDemuxer, Variant, VariantInfo};
pub(crate) mod abr;
pub(crate) mod crypto;
pub(crate) mod segment;
pub(crate) mod http;
pub(crate) mod track;
#[cfg(test)]
#[path = "../../tests/support/server.rs"]
pub(crate) mod test_server;
