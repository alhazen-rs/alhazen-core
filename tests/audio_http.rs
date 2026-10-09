//! Audio files over HTTP: a seek near the end reads from near the end (Xing TOC, byte estimates,
//! FLAC seek table or bisection, Ogg bisection), never the whole file again.
#![cfg(all(feature = "native", feature = "http"))]

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alhazen_core::Source;
use alhazen_core::backend::Registry;
use alhazen_core::demux::StreamKind;

/// Serves `bytes` with Range support; records the start offset of every request.
fn serve(bytes: Vec<u8>, name: &str) -> (String, Arc<Mutex<Vec<u64>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    let bytes = Arc::new(bytes);
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let len = bytes.len();
            let (start, end) = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Range"))
                .and_then(|h| {
                    let (a, b) = h.value.as_str().strip_prefix("bytes=")?.split_once('-')?;
                    Some((a.parse::<usize>().ok()?, b.parse::<usize>().map(|e| e + 1).unwrap_or(len)))
                })
                .unwrap_or((0, len));
            let (start, end) = (start.min(len), end.min(len));
            log.lock().unwrap().push(start as u64);
            let body = std::io::Cursor::new(bytes[start..end].to_vec());
            let headers = vec![
                tiny_http::Header::from_bytes("Accept-Ranges", "bytes").unwrap(),
                tiny_http::Header::from_bytes("Content-Range", format!("bytes {start}-{}/{len}", end.max(start + 1) - 1)).unwrap(),
            ];
            let _ = req.respond(tiny_http::Response::new(tiny_http::StatusCode(206), headers, body, Some(end - start), None));
        }
    });
    (format!("http://{addr}/{name}"), requests)
}

/// A minute of a two-tone chirp encoded with `codec` (ffmpeg arguments); `None` without ffmpeg.
fn long_file(name: &str, codec: &[&str]) -> Option<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("aevalsrc=exprs='0.5*sin(2*PI*(220+30*t)*t)|0.4*sin(2*PI*(330+20*t)*t)':s=44100:d=60")
        .args(codec)
        .arg(&path)
        .status()
        .ok()?
        .success();
    ok.then(|| std::fs::read(&path).unwrap())
}

#[test]
fn seeking_near_the_end_reads_from_near_the_end() {
    let cases: [(&str, &[&str], Duration); 6] = [
        ("long.mp3", &["-c:a", "libmp3lame", "-b:a", "128k"], Duration::from_millis(700)),
        ("long_cbr.mp3", &["-c:a", "libmp3lame", "-b:a", "128k", "-write_xing", "0"], Duration::from_millis(700)),
        ("long.aac", &["-c:a", "aac", "-b:a", "128k", "-f", "adts"], Duration::from_millis(700)),
        ("long.flac", &["-c:a", "flac"], Duration::from_millis(200)),
        ("long.opus", &["-c:a", "libopus", "-b:a", "96k", "-ar", "48000"], Duration::from_millis(2300)),
        ("long.ogg", &["-c:a", "libvorbis", "-q:a", "4"], Duration::from_millis(2300)),
    ];
    for (name, codec, tolerance) in cases {
        let Some(bytes) = long_file(name, codec) else {
            eprintln!("skipped: no ffmpeg");
            return;
        };
        let len = bytes.len() as u64;
        let (url, requests) = serve(bytes, name);
        let source = Source::parse(&url).unwrap();
        let mut src = source.open().unwrap();
        let format = alhazen_core::demux::probe(src.as_mut()).unwrap().expect(name);
        let mut d = Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap();
        let s = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
        let duration = s.duration.unwrap_or_else(|| panic!("{name}: duration over HTTP"));
        assert!(duration.abs_diff(Duration::from_secs(60)) < Duration::from_secs(2), "{name}: duration {duration:?}");
        let target = duration.mul_f64(0.8);
        let before = requests.lock().unwrap().len();
        d.seek(target).unwrap();
        let first = d.next_packet().unwrap().expect("audio after the seek");
        assert!(first.pts.abs_diff(target) <= tolerance, "{name}: landed at {:?} for {target:?}", first.pts);
        let during: Vec<u64> = requests.lock().unwrap()[before..].to_vec();
        assert!(during.iter().all(|&start| start >= len * 4 / 10), "{name}: a request during the seek started at {during:?} of {len}");
    }
}
