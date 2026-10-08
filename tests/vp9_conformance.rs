//! libvpx VP9 conformance vectors through vp9-mt at several thread counts: every frame's MD5
//! must match libvpx's. The vectors are downloaded by `scripts/fetch_vp9_vectors.py`; set
//! `VP9_VECTORS_DIR` to run (CI does). Without it the test reports that it was skipped.
#![cfg(feature = "native")]

use md5::{Digest, Md5};
use alhazen_core::demux::{Demuxer, MatroskaDemuxer, StreamKind};
use alhazen_core::source::FileSource;

fn packets(path: &std::path::Path) -> Vec<Vec<u8>> {
    let mut d = MatroskaDemuxer::open(Box::new(FileSource::open(path).unwrap())).unwrap();
    let v = d.streams().iter().find(|s| s.kind == StreamKind::Video).unwrap().id;
    let mut out = vec![];
    while let Some(p) = d.next_packet().unwrap() {
        if p.stream == v {
            out.push(p.data);
        }
    }
    out
}

/// libvpx's `--md5` per frame: visible rows of Y, U, V; 16-bit little-endian samples above 8 bits.
fn frame_md5s(packets: &[Vec<u8>], threads: usize) -> Vec<String> {
    let mut dec = vp9_mt::Vp9Decoder::with_threads(threads);
    let mut out = vec![];
    let mut take = |f: vp9_mt::DecodedFrame| {
        let mut h = Md5::new();
        for p in &f.planes {
            h.update(p);
        }
        out.push(format!("{:x}", h.finalize()));
    };
    for p in packets {
        dec.push(p, None).unwrap();
        while let Ok(f) = dec.next_frame() {
            take(f);
        }
    }
    dec.flush();
    while let Ok(f) = dec.next_frame() {
        take(f);
    }
    out
}

#[test]
fn vp9_conformance_vectors_match_libvpx_at_every_thread_count() {
    let Ok(dir) = std::env::var("VP9_VECTORS_DIR") else {
        eprintln!("skipped: set VP9_VECTORS_DIR (see scripts/fetch_vp9_vectors.py)");
        return;
    };
    // Tests run in the crate directory; a relative path means the workspace root.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(dir);
    let dir = dir.display().to_string();
    let mut vectors: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "webm"))
        .collect();
    vectors.sort();
    assert!(!vectors.is_empty(), "no vectors in {dir}");
    let mut failures = vec![];
    for path in &vectors {
        let expected: Vec<String> = std::fs::read_to_string(path.with_extension("webm.md5"))
            .unwrap()
            .lines()
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect();
        let pk = packets(path);
        for threads in [1, 2, 4, 8, 16] {
            let got = frame_md5s(&pk, threads);
            if got != expected {
                let first = got.iter().zip(&expected).position(|(a, b)| a != b);
                failures.push(format!(
                    "{} threads={threads}: {} frames vs {} expected, first mismatch at {first:?}",
                    path.file_name().unwrap().to_string_lossy(),
                    got.len(),
                    expected.len()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
