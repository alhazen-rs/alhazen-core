//! Downloads for HLS: playlists, keys, init sections and segments, with retries.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ureq::ResponseExt;
use url::Url;

use crate::{Error, Result};

/// Segments larger than this are refused.
const MAX_BODY: usize = 32 << 20;
/// Waits before each retry of a transient failure.
const RETRY_DELAYS: [Duration; 3] = [Duration::from_millis(500), Duration::from_secs(1), Duration::from_secs(2)];
const CHUNK: usize = 64 * 1024;

pub(crate) struct Fetched {
    pub data: Vec<u8>,
    /// The URL after redirects: relative URIs inside a playlist resolve against it.
    pub url: Url,
    pub elapsed: Duration,
}

/// How a failed attempt may be retried.
enum Retry {
    /// Network trouble, 5xx, 429: up to three more times.
    Transient,
    /// 403/404/410: once (often an expired signed URL, rarely a glitch).
    Once,
    Never,
}

pub(crate) struct Fetcher {
    agent: ureq::Agent,
    cancel: Arc<AtomicBool>,
}

impl Fetcher {
    pub fn new(cancel: Arc<AtomicBool>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(15)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .build()
            .new_agent();
        Self { agent, cancel }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// `url` (bytes `offset..offset + len` of it when `range` is given), retried as needed.
    pub fn get(&self, url: &Url, range: Option<(u64, u64)>) -> Result<Fetched> {
        let mut attempt = 0;
        loop {
            let (err, retry) = match self.once(url, range) {
                Ok(f) => return Ok(f),
                Err(e) => e,
            };
            let allowed = match retry {
                Retry::Transient => RETRY_DELAYS.len(),
                Retry::Once => 1,
                Retry::Never => 0,
            };
            if attempt >= allowed || self.cancelled() {
                return Err(err);
            }
            log::info!("HLS: {err}; retrying");
            let until = Instant::now() + RETRY_DELAYS[attempt];
            while Instant::now() < until {
                if self.cancelled() {
                    return Err(err);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            attempt += 1;
        }
    }

    fn once(&self, url: &Url, range: Option<(u64, u64)>) -> std::result::Result<Fetched, (Error, Retry)> {
        let started = Instant::now();
        let mut req = self.agent.get(url.as_str());
        if let Some((offset, len)) = range {
            req = req.header("Range", format!("bytes={offset}-{}", offset + len.max(1) - 1));
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(code)) => {
                let retry = match code {
                    429 | 500..=599 => Retry::Transient,
                    403 | 404 | 410 => Retry::Once,
                    _ => Retry::Never,
                };
                return Err((Error::Http(format!("{url}: HTTP {code}")), retry));
            }
            Err(e) => return Err((Error::Http(format!("{url}: {e}")), Retry::Transient)),
        };
        let final_url = Url::parse(&resp.get_uri().to_string()).unwrap_or_else(|_| url.clone());
        let mut body = resp.into_body().into_reader();
        let mut data = Vec::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            if self.cancelled() {
                return Err((Error::Http("cancelled".into()), Retry::Never));
            }
            let n = body.read(&mut buf).map_err(|e| (Error::Http(format!("{url}: {e}")), Retry::Transient))?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.len() > MAX_BODY {
                return Err((Error::Http(format!("{url}: segment larger than 32 MiB")), Retry::Never));
            }
        }
        Ok(Fetched { data, url: final_url, elapsed: started.elapsed() })
    }
}
