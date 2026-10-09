//! Byte-level media inputs.

mod file;
mod memory;
#[cfg(feature = "http")]
mod http;

use std::io::{Read, Seek};
use std::path::PathBuf;

pub use file::FileSource;
pub use memory::MemorySource;
#[cfg(feature = "http")]
pub use http::HttpSource;
use url::Url;

use crate::{Error, Result};

/// A readable, (usually) seekable stream of container bytes.
pub trait MediaSource: Read + Seek + Send {
    /// Total length in bytes, if known.
    fn byte_len(&self) -> Option<u64>;
    /// Whether `seek` to arbitrary positions is supported.
    fn is_seekable(&self) -> bool;
    /// Live sources have no fixed duration and cannot seek.
    fn is_live(&self) -> bool;
    /// Human readable description for errors and logs.
    fn description(&self) -> String;
    /// A file on this machine: scanning it costs little (readers may build exact seek indexes,
    /// read trailing tags). `false` for network sources.
    fn is_local(&self) -> bool {
        false
    }
}

/// Where media comes from. `Player::open` takes this rather than a `MediaSource`
/// because adaptive streams (HLS/DASH) are segment-based, not byte-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Http(Url),
    /// HLS (`.m3u8`) or DASH (`.mpd`). Not playable until phase 5.
    Adaptive(Url),
}

impl Source {
    /// Classifies a path or URL string.
    pub fn parse(s: &str) -> Result<Source> {
        let s = s.trim();
        if s.is_empty() {
            return Err(Error::InvalidSource("empty source".into()));
        }
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            let url = Url::parse(s).map_err(|e| Error::InvalidSource(format!("{s}: {e}")))?;
            let path = url.path().to_ascii_lowercase();
            if path.ends_with(".m3u8") || path.ends_with(".mpd") {
                return Ok(Source::Adaptive(url));
            }
            return Ok(Source::Http(url));
        }
        if lower.starts_with("file://") {
            let url = Url::parse(s).map_err(|e| Error::InvalidSource(format!("{s}: {e}")))?;
            let path = url
                .to_file_path()
                .map_err(|_| Error::InvalidSource(format!("{s}: not a local file URL")))?;
            return Ok(Source::File(path));
        }
        if lower.contains("://") {
            return Err(Error::InvalidSource(format!("{s}: unsupported URL scheme")));
        }
        Ok(Source::File(PathBuf::from(s)))
    }

    /// Opens a byte-level source. Adaptive sources are rejected until phase 5.
    pub fn open(&self) -> Result<Box<dyn MediaSource>> {
        match self {
            Source::File(path) => Ok(Box::new(FileSource::open(path)?)),
            #[cfg(feature = "http")]
            Source::Http(url) => Ok(Box::new(HttpSource::open(url.clone())?)),
            #[cfg(not(feature = "http"))]
            Source::Http(_) => Err(Error::Unsupported("HTTP sources (enable the `http` feature)")),
            Source::Adaptive(_) => Err(Error::Unsupported("adaptive streaming (HLS/DASH)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_path_as_file() {
        assert_eq!(Source::parse("movie.webm").unwrap(), Source::File("movie.webm".into()));
        assert_eq!(
            Source::parse("C:\\videos\\a.mp4").unwrap(),
            Source::File("C:\\videos\\a.mp4".into())
        );
    }

    #[test]
    fn parses_http_and_adaptive_urls() {
        assert!(matches!(Source::parse("https://x.org/a.webm").unwrap(), Source::Http(_)));
        assert!(matches!(Source::parse("HTTP://x.org/a.mp4?x=1").unwrap(), Source::Http(_)));
        assert!(matches!(Source::parse("https://x.org/live.m3u8").unwrap(), Source::Adaptive(_)));
        assert!(matches!(Source::parse("https://x.org/a.MPD?t=1").unwrap(), Source::Adaptive(_)));
    }

    #[test]
    fn rejects_empty_and_unknown_schemes() {
        assert!(matches!(Source::parse("  "), Err(Error::InvalidSource(_))));
        assert!(matches!(Source::parse("rtsp://cam/1"), Err(Error::InvalidSource(_))));
    }

    #[test]
    fn adaptive_open_is_unsupported_for_now() {
        let src = Source::parse("https://x.org/a.m3u8").unwrap();
        assert!(matches!(src.open(), Err(Error::Unsupported(_))));
    }
}
