//! An in-process HTTP server for HLS tests: serves a directory (with `Range`), and can fail,
//! stall or throttle given paths, and present a VOD playlist as a sliding live window.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    /// path → (status, times left; `usize::MAX` = always).
    failures: HashMap<String, (u16, usize)>,
    /// path → how long the body stalls before its first byte.
    stalls: HashMap<String, Duration>,
    /// Bytes per second for every body.
    throttle: Option<u64>,
    hits: HashMap<String, usize>,
    /// path → replacement body (playlists rewritten by a test).
    bodies: HashMap<String, Vec<u8>>,
    /// path → path it redirects to (302).
    redirects: HashMap<String, String>,
    live: Option<Live>,
}

struct Live {
    path: String,
    header: String,
    /// `#EXTINF` line + URI line of every source segment.
    segments: Vec<(String, String)>,
    window: usize,
    started: Instant,
    every: Duration,
    /// The window stops moving after this many advances (the stream "stalls").
    advances: usize,
}

pub struct Server {
    /// `http://127.0.0.1:port/`
    pub base: String,
    state: Arc<Mutex<State>>,
}

impl Server {
    /// Serves files under `root`.
    pub fn dir(root: impl Into<PathBuf>) -> Server {
        let root = root.into();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", server.server_addr().to_ip().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let s = state.clone();
        thread::spawn(move || {
            for req in server.incoming_requests() {
                let (root, s) = (root.clone(), s.clone());
                thread::spawn(move || respond(req, &root, &s));
            }
        });
        Server { base, state }
    }

    /// The URL of `path` on this server.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The next `times` requests of `path` get `status` (`usize::MAX`: all of them).
    pub fn fail(&self, path: &str, status: u16, times: usize) {
        self.state.lock().unwrap().failures.insert(path.into(), (status, times));
    }

    /// `path`'s body stalls for `d` before its first byte (headers are sent at once).
    pub fn stall(&self, path: &str, d: Duration) {
        self.state.lock().unwrap().stalls.insert(path.into(), d);
    }

    pub fn throttle(&self, bytes_per_second: Option<u64>) {
        self.state.lock().unwrap().throttle = bytes_per_second;
    }

    /// Serves `body` for `path` instead of the file.
    pub fn set_body(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.state.lock().unwrap().bodies.insert(path.into(), body.into());
    }

    /// Requests for `from` are redirected (302) to `to`.
    pub fn redirect(&self, from: &str, to: &str) {
        self.state.lock().unwrap().redirects.insert(from.into(), to.into());
    }

    pub fn hits(&self, path: &str) -> usize {
        self.state.lock().unwrap().hits.get(path).copied().unwrap_or(0)
    }

    /// Serves `path` as a live playlist: a window of `window` segments of the VOD playlist
    /// `source` (a file under the root), moving one segment every `every`, `advances` times.
    pub fn live(&self, path: &str, source: &str, root: impl Into<PathBuf>, window: usize, every: Duration, advances: usize) {
        let text = std::fs::read_to_string(root.into().join(source)).unwrap();
        let mut header = String::new();
        let mut segments = Vec::new();
        let mut lines = text.lines();
        while let Some(l) = lines.next() {
            if l.starts_with("#EXTINF") {
                segments.push((l.to_owned(), lines.next().unwrap().to_owned()));
            } else if l.starts_with("#EXT-X-TARGETDURATION") || l.starts_with("#EXT-X-VERSION") || l == "#EXTM3U" {
                header.push_str(l);
                header.push('\n');
            }
        }
        assert!(segments.len() >= window + advances, "not enough segments for that live window");
        self.state.lock().unwrap().live =
            Some(Live { path: path.into(), header, segments, window, started: Instant::now(), every, advances });
    }
}

fn live_playlist(l: &Live) -> String {
    let n = ((l.started.elapsed().as_secs_f64() / l.every.as_secs_f64()) as usize).min(l.advances);
    let mut out = format!("{}#EXT-X-MEDIA-SEQUENCE:{n}\n", l.header);
    for (inf, uri) in &l.segments[n..n + l.window] {
        out.push_str(&format!("{inf}\n{uri}\n"));
    }
    out
}

fn respond(req: tiny_http::Request, root: &std::path::Path, state: &Mutex<State>) {
    let path = req.url().trim_start_matches('/').split('?').next().unwrap_or("").to_owned();
    let range = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .and_then(|h| {
            let (a, b) = h.value.as_str().strip_prefix("bytes=")?.split_once('-')?;
            Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()))
        });
    let (body, failure, stall, throttle) = {
        let mut s = state.lock().unwrap();
        *s.hits.entry(path.clone()).or_default() += 1;
        if let Some(to) = s.redirects.get(&path) {
            let location = tiny_http::Header::from_bytes("Location", format!("/{to}")).unwrap();
            drop(s);
            let _ = req.respond(tiny_http::Response::empty(302).with_header(location));
            return;
        }
        let failure = match s.failures.get_mut(&path) {
            Some((status, left)) if *left > 0 => {
                if *left != usize::MAX {
                    *left -= 1;
                }
                Some(*status)
            }
            _ => None,
        };
        let body = match &s.live {
            Some(l) if l.path == path => Some(live_playlist(l).into_bytes()),
            _ => s.bodies.get(&path).cloned(),
        };
        (body, failure, s.stalls.get(&path).copied(), s.throttle)
    };
    if let Some(status) = failure {
        let _ = req.respond(tiny_http::Response::from_string("failure").with_status_code(status));
        return;
    }
    let Some(mut body) = body.or_else(|| std::fs::read(root.join(&path)).ok()) else {
        let _ = req.respond(tiny_http::Response::from_string("not found").with_status_code(404));
        return;
    };
    let mut status = 200;
    let total = body.len();
    let mut headers = vec![tiny_http::Header::from_bytes("Accept-Ranges", "bytes").unwrap()];
    if let Some((a, b)) = range {
        let end = b.map_or(total, |b| (b + 1).min(total));
        let a = a.min(end);
        body = body[a..end].to_vec();
        status = 206;
        headers.push(tiny_http::Header::from_bytes("Content-Range", format!("bytes {a}-{}/{total}", end.max(1) - 1)).unwrap());
    }
    let len = body.len();
    let reader = Slow { data: body, pos: 0, stall, throttle, started: None };
    let resp = tiny_http::Response::new(status.into(), headers, reader, Some(len), None);
    let _ = req.respond(resp);
}

/// A body that may stall before its first byte and trickle at a set rate.
struct Slow {
    data: Vec<u8>,
    pos: usize,
    stall: Option<Duration>,
    throttle: Option<u64>,
    started: Option<Instant>,
}

impl Read for Slow {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(d) = self.stall.take() {
            thread::sleep(d);
        }
        let started = *self.started.get_or_insert_with(Instant::now);
        let mut n = buf.len().min(self.data.len() - self.pos);
        if let Some(rate) = self.throttle {
            n = n.min((rate / 20).max(1) as usize);
            // Never ahead of the allowed rate.
            let due = Duration::from_secs_f64((self.pos + n) as f64 / rate as f64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                thread::sleep(wait);
            }
        }
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
