use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::thread;
use std::time::Duration;

use url::Url;

use super::MediaSource;
use crate::{Error, Result};

const READ_AHEAD: usize = 1 << 20;
/// Forward seeks up to this far are served by reading and discarding instead of a new request.
const SKIP_BY_READING: u64 = 256 * 1024;
const RETRIES: u32 = 3;
/// The first bytes of the stream are kept so that rewinding into them (container probing)
/// works even when the server does not support `Range`.
const HEAD_CACHE: usize = 256 * 1024;

/// A progressive HTTP(S) download using `Range` requests for seeking.
pub struct HttpSource {
    url: Url,
    agent: ureq::Agent,
    len: Option<u64>,
    seekable: bool,
    /// Logical read position.
    pos: u64,
    /// Position of `body` in the stream. `pos < body_pos` means reads come from `head`.
    body_pos: u64,
    /// Bytes `0..head.len()` of the stream, while they were read contiguously from the start.
    head: Vec<u8>,
    body: Option<BufReader<ureq::BodyReader<'static>>>,
}

impl HttpSource {
    pub fn open(url: Url) -> Result<Self> {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(15)))
            .build()
            .new_agent();
        let mut src = Self {
            url,
            agent,
            len: None,
            seekable: false,
            pos: 0,
            body_pos: 0,
            head: Vec::new(),
            body: None,
        };
        let resp = src.request(0).map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
        };
        match status {
            206 => {
                src.seekable = true;
                src.len = header("content-range").as_deref().and_then(parse_content_range_total);
            }
            200 => {
                src.seekable = header("accept-ranges").is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
                src.len = header("content-length").and_then(|v| v.parse().ok());
            }
            other => return Err(Error::Http(format!("unexpected status {other}"))),
        }
        src.body = Some(BufReader::with_capacity(READ_AHEAD, resp.into_body().into_reader()));
        Ok(src)
    }

    fn request(&self, from: u64) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.agent
            .get(self.url.as_str())
            .header("Range", format!("bytes={from}-"))
            .call()
    }

    /// (Re)connects at `self.pos`, retrying with exponential backoff.
    fn connect(&mut self) -> io::Result<()> {
        let mut last_err = None;
        for attempt in 0..RETRIES {
            if attempt > 0 {
                thread::sleep(Duration::from_millis(200 << attempt));
            }
            match self.request(self.pos) {
                Ok(resp) if resp.status().as_u16() == 206 || self.pos == 0 => {
                    self.body_pos = self.pos;
                    self.body = Some(BufReader::with_capacity(READ_AHEAD, resp.into_body().into_reader()));
                    return Ok(());
                }
                Ok(resp) => {
                    return Err(io::Error::other(format!(
                        "server ignored Range request (status {})",
                        resp.status().as_u16()
                    )));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(io::Error::other(format!("HTTP reconnect failed: {}", last_err.unwrap())))
    }

    fn at_end(&self) -> bool {
        self.len.is_some_and(|len| self.pos >= len)
    }
}

impl Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.at_end() {
            return Ok(0);
        }
        if self.pos < self.body_pos {
            // Rewound into the cached head of the stream.
            let start = self.pos as usize;
            let n = buf.len().min(self.body_pos as usize - start);
            buf[..n].copy_from_slice(&self.head[start..start + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        for _ in 0..RETRIES {
            if self.body.is_none() {
                self.connect()?;
            }
            match self.body.as_mut().unwrap().read(buf) {
                // A 0-byte read before the known end means the connection dropped.
                Ok(0) if !self.at_end() && self.len.is_some() && self.seekable => self.body = None,
                Ok(n) => {
                    if self.head.len() as u64 == self.body_pos && self.head.len() < HEAD_CACHE {
                        let keep = n.min(HEAD_CACHE - self.head.len());
                        self.head.extend_from_slice(&buf[..keep]);
                    }
                    self.pos += n as u64;
                    self.body_pos = self.pos;
                    return Ok(n);
                }
                Err(_) if self.seekable => self.body = None,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("HTTP stream kept dropping"))
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(invalid_seek)?,
            SeekFrom::End(d) => {
                let len = self.len.ok_or_else(|| io::Error::other("length unknown"))?;
                len.checked_add_signed(d).ok_or_else(invalid_seek)?
            }
        };
        if target == self.pos {
            return Ok(target);
        }
        if target <= self.body_pos && self.body_pos <= self.head.len() as u64 {
            // Everything up to the body position is cached: no request needed.
            self.pos = target;
            return Ok(target);
        }
        // Past the cache: continue from the body position.
        self.pos = self.pos.max(self.body_pos);
        let forward = target.saturating_sub(self.pos);
        if target > self.pos && (forward <= SKIP_BY_READING || !self.seekable) && !self.at_end() {
            let copied = io::copy(&mut self.by_ref().take(forward), &mut io::sink())?;
            if copied == forward {
                return Ok(target);
            }
        }
        if !self.seekable {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "source is not seekable"));
        }
        self.pos = target;
        self.body_pos = target;
        self.body = None; // reconnect lazily on next read
        Ok(target)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.pos)
    }
}

impl MediaSource for HttpSource {
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
    fn is_seekable(&self) -> bool {
        self.seekable
    }
    fn is_live(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        self.url.to_string()
    }
}

fn invalid_seek() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "seek out of range")
}

/// `bytes 0-99/1234` -> `Some(1234)`; `bytes 0-99/*` -> `None`.
fn parse_content_range_total(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(parse_content_range_total("bytes 0-99/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 0-99/*"), None);
    }
}
