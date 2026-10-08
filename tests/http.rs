//! `HttpSource` and player-over-HTTP tests against an in-process server.
#![cfg(feature = "http")]

use std::io::{Read, Seek, SeekFrom};
use std::thread;

use alhazen_core::source::{HttpSource, MediaSource};

/// Serves `bytes` forever; honors `Range: bytes=N-` only if `ranges` is true.
fn serve(bytes: Vec<u8>, ranges: bool) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let start = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Range"))
                .and_then(|h| h.value.as_str().strip_prefix("bytes=")?.trim_end_matches('-').parse::<usize>().ok())
                .filter(|_| ranges)
                .unwrap_or(0);
            let body = bytes[start.min(bytes.len())..].to_vec();
            let mut resp = tiny_http::Response::from_data(body);
            if ranges {
                resp.add_header(tiny_http::Header::from_bytes("Accept-Ranges", "bytes").unwrap());
                let range = format!("bytes {start}-{}/{}", bytes.len() - 1, bytes.len());
                resp.add_header(tiny_http::Header::from_bytes("Content-Range", range).unwrap());
                resp = resp.with_status_code(206);
            }
            let _ = req.respond(resp);
        }
    });
    format!("http://{addr}/av1.webm")
}

fn fixture_bytes() -> Vec<u8> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/av1.webm")).unwrap()
}

#[test]
fn range_server_is_seekable_with_known_length() {
    let bytes = fixture_bytes();
    let mut src = HttpSource::open(url::Url::parse(&serve(bytes.clone(), true)).unwrap()).unwrap();
    assert!(src.is_seekable());
    assert_eq!(src.byte_len(), Some(bytes.len() as u64));
    src.seek(SeekFrom::Start(40_000)).unwrap();
    let mut buf = [0u8; 16];
    src.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &bytes[40_000..40_016]);
    src.seek(SeekFrom::Start(4)).unwrap();
    src.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &bytes[4..20]);
}

#[test]
fn server_without_ranges_rewinds_only_within_cached_head() {
    // Larger than the 256 KiB head cache.
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut src = HttpSource::open(url::Url::parse(&serve(bytes.clone(), false)).unwrap()).unwrap();
    assert!(!src.is_seekable());
    src.seek(SeekFrom::Start(100)).unwrap(); // forward: served by reading
    let mut buf = [0u8; 4];
    src.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &bytes[100..104]);
    // Rewinding into the cached head works without Range support (container probing needs it).
    src.seek(SeekFrom::Start(0)).unwrap();
    src.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &bytes[0..4]);
    // Past the cache, backward seeks are impossible.
    src.seek(SeekFrom::Start(299_000)).unwrap();
    src.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &bytes[299_000..299_004]);
    assert!(src.seek(SeekFrom::Start(0)).is_err());
}

#[cfg(feature = "native")]
#[test]
fn player_plays_over_http() {
    use std::time::{Duration, Instant};
    use alhazen_core::{Player, PlayerConfig, Source};
    let url = serve(fixture_bytes(), true);
    let player = Player::open(Source::parse(&url).unwrap(), PlayerConfig::default()).unwrap();
    let start = Instant::now();
    while player.current_frame().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5));
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(feature = "native")]
#[test]
fn player_plays_from_server_without_ranges() {
    use std::time::{Duration, Instant};
    use alhazen_core::{Player, PlayerConfig, Source};
    let url = serve(fixture_bytes(), false);
    let player = Player::open(Source::parse(&url).unwrap(), PlayerConfig::default()).unwrap();
    let start = Instant::now();
    while player.current_frame().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5), "no frame from a server without Range support");
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(feature = "native")]
#[test]
fn seeking_a_non_seekable_stream_is_refused_without_killing_the_player() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use alhazen_core::clock::MockClock;
    use alhazen_core::{Player, PlayerConfig, PlayerEvent, PlayerState, Source};
    let url = serve(fixture_bytes(), false);
    let clock = Arc::new(MockClock::new());
    let config = PlayerConfig { clock: Some(clock.clone()), ..Default::default() };
    let player = Player::open(Source::parse(&url).unwrap(), config).unwrap();
    let events = player.events();
    assert!(!player.is_seekable());

    player.seek(Duration::from_secs(1));
    assert_eq!(player.position(), Duration::ZERO, "seek must be a no-op");
    assert!(!player.state().is_error());
    assert!(events.try_iter().any(|e| matches!(e, PlayerEvent::Warning(_))));

    // Play to the end; Play again cannot restart (that needs a seek) but must not fail.
    player.play();
    let start = Instant::now();
    while player.state() != PlayerState::Ended {
        assert!(start.elapsed() < Duration::from_secs(10), "never ended");
        player.current_frame();
        clock.advance(Duration::from_millis(20));
        thread::sleep(Duration::from_millis(1));
    }
    player.play();
    assert_eq!(player.state(), PlayerState::Ended);
}

/// Serves the first `stall_after` bytes of `bytes`, then stops sending without closing.
#[cfg(feature = "native")]
fn serve_stalling(bytes: Vec<u8>, stall_after: usize) -> String {
    struct Stall {
        data: std::io::Cursor<Vec<u8>>,
    }
    impl Read for Stall {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.data.read(buf)?;
            if n == 0 {
                thread::sleep(std::time::Duration::from_secs(3600));
            }
            Ok(n)
        }
    }
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let len = bytes.len();
            let reader = Stall { data: std::io::Cursor::new(bytes[..stall_after].to_vec()) };
            let resp = tiny_http::Response::new(200.into(), vec![], reader, Some(len), None);
            thread::spawn(move || {
                let _ = req.respond(resp);
            });
        }
    });
    format!("http://{addr}/stall.webm")
}

#[cfg(feature = "native")]
#[test]
fn dropping_player_with_stalled_http_read_returns_promptly() {
    use std::time::Duration;
    use alhazen_core::{Player, PlayerConfig, Source};
    let url = serve_stalling(fixture_bytes(), 30_000);
    let player = Player::open(Source::parse(&url).unwrap(), PlayerConfig::default()).unwrap();
    thread::sleep(Duration::from_millis(300)); // let the demux thread block in the stalled read
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        drop(player);
        let _ = tx.send(());
    });
    assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), "Player::drop hung on a stalled HTTP read");
}
