use std::fmt;

/// Every error `alhazen-core` can produce.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("invalid source: {0}")]
    InvalidSource(String),
    #[error("unsupported container format")]
    UnsupportedContainer,
    #[error("unsupported codec {codec} (tried backends: {})", TriedList(.tried_backends))]
    UnsupportedCodec {
        codec: String,
        tried_backends: Vec<&'static str>,
    },
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("demux error: {0}")]
    Demux(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("seek error: {0}")]
    Seek(String),
}

pub type Result<T> = std::result::Result<T, Error>;

struct TriedList<'a>(&'a [&'static str]);

impl fmt::Display for TriedList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&self.0.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_codec_lists_backends() {
        let err = Error::UnsupportedCodec {
            codec: "VP9".into(),
            tried_backends: vec!["native"],
        };
        assert_eq!(err.to_string(), "unsupported codec VP9 (tried backends: native)");
        let err = Error::UnsupportedCodec {
            codec: "VP9".into(),
            tried_backends: vec![],
        };
        assert_eq!(err.to_string(), "unsupported codec VP9 (tried backends: none)");
    }
}
