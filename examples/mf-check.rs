//! `mf-check <fixtures-dir>`: decodes the test fixtures through Windows' own decoders (Media
//! Foundation) and reports, per file, which decoder ran, GPU or software, and PASS/FAIL. Built in
//! CI and run from `prebuilt/` on a Windows machine without a Rust toolchain.

#[cfg(not(all(windows, feature = "media-foundation", feature = "native")))]
fn main() {
    eprintln!("mf-check needs Windows and the media-foundation + native features");
    std::process::exit(2);
}

#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn main() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::time::{Duration, Instant};
    use video_core::backend::Registry;
    use video_core::decode::{AudioDecoder, DecodedFrame, VideoDecoder};
    use video_core::demux::StreamKind;
    use video_core::mf::select::MfCodec;
    use video_core::mf::{MfAudioDecoder, MfVideoDecoder};
    use video_core::Source;

    let dir = std::env::args().nth(1).unwrap_or_else(|| "crates/video-core/tests/fixtures".into());
    // (file, track kind, expected frames or audio sample frames, tolerance)
    let cases: &[(&str, StreamKind, usize, usize)] = &[
        ("h264_aac.mp4", StreamKind::Video, 30, 0),
        ("hevc.mkv", StreamKind::Video, 30, 0),
        ("hevc_10bit.mp4", StreamKind::Video, 30, 0),
        ("vp9_profile0.webm", StreamKind::Video, 60, 0),
        ("vp9_10bit.webm", StreamKind::Video, 60, 0),
        ("av1.webm", StreamKind::Video, 60, 0),
        ("h264_aac.mp4", StreamKind::Audio, 44_100, 4096),
        ("mp3.mkv", StreamKind::Audio, 24_000, 4096),
        ("mp3.mp4", StreamKind::Audio, 24_000, 4096),
        ("ac3.mkv", StreamKind::Audio, 24_000, 4096),
        ("eac3.mkv", StreamKind::Audio, 24_000, 4096),
        ("flac.mkv", StreamKind::Audio, 24_000, 4096),
        ("alac.m4a", StreamKind::Audio, 24_000, 4096),
    ];
    let mut failed = 0;
    println!("{:<18} {:<6} {:<8} {:<44} {:>8} {:>8}  result", "file", "track", "decoder", "", "count", "ms");
    for &(file, kind, expect, tolerance) in cases {
        let path = format!("{dir}/{file}");
        // (packets sent, frames/samples out, phase: 0 send, 1 receive, 2 drain) for the hang report.
        let progress = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)]);
        let p2 = progress.clone();
        let run = move || -> Result<(String, usize), String> {
            let progress = p2;
            let source = Source::parse(&path).map_err(|e| e.to_string())?;
            let mut src = source.open().map_err(|e| e.to_string())?;
            let format = video_core::demux::probe(src.as_mut()).map_err(|e| e.to_string())?.ok_or("not a media file")?;
            let mut d = Registry::empty_with_native().open_demuxer(&source, format, src, None).map_err(|e| e.to_string())?;
            let s = d.streams().iter().find(|s| s.kind == kind).cloned().ok_or("no such track")?;
            let codec = MfCodec::of(&s).ok_or("not a Media Foundation codec")?;
            let mut count = 0;
            if kind == StreamKind::Video {
                let mut dec = MfVideoDecoder::new(codec, &s, true).map_err(|e| e.to_string())?;
                while let Some(p) = d.next_packet().map_err(|e| e.to_string())? {
                    if p.stream == s.id {
                        dec.send_packet(&p).map_err(|e| e.to_string())?;
                        while let Some(DecodedFrame::Yuv(_)) = dec.receive_frame().map_err(|e| e.to_string())? {
                            count += 1;
                        }
                    }
                }
                dec.send_eof();
                while let Some(DecodedFrame::Yuv(_)) = dec.receive_frame().map_err(|e| e.to_string())? {
                    count += 1;
                }
                let (name, gpu) = dec.description().unwrap_or_default();
                Ok((format!("{} {name}", if gpu { "GPU " } else { "soft" }), count))
            } else {
                let mut dec = MfAudioDecoder::new(codec, &s).map_err(|e| e.to_string())?;
                while let Some(p) = d.next_packet().map_err(|e| e.to_string())? {
                    if p.stream == s.id {
                        progress[2].store(0, Relaxed);
                        dec.send_packet(&p).map_err(|e| e.to_string())?;
                        progress[0].fetch_add(1, Relaxed);
                        progress[2].store(1, Relaxed);
                        while let Some(b) = dec.receive_samples().map_err(|e| e.to_string())? {
                            count += b.samples.len() / b.channels as usize;
                            progress[1].store(count, Relaxed);
                        }
                    }
                }
                progress[2].store(2, Relaxed);
                dec.send_eof();
                while let Some(b) = dec.receive_samples().map_err(|e| e.to_string())? {
                    count += b.samples.len() / b.channels as usize;
                    progress[1].store(count, Relaxed);
                }
                Ok((format!("soft {}", dec.description().unwrap_or_default()), count))
            }
        };
        let track = if kind == StreamKind::Video { "video" } else { "audio" };
        // Printed before decoding so a decoder that hangs or crashes the process is named.
        print!("{file:<18} {track:<6} ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let start = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(run()));
        let Ok(result) = rx.recv_timeout(Duration::from_secs(10)) else {
            // The decode thread is stuck inside the decoder; leave it and test the next file.
            failed += 1;
            let phase = ["sending a packet", "pulling output", "draining"][progress[2].load(Relaxed)];
            println!(
                "{:<53} {:>8} {:>8}  HANG while {phase} ({} packets in)",
                "-",
                progress[1].load(Relaxed),
                start.elapsed().as_millis(),
                progress[0].load(Relaxed)
            );
            continue;
        };
        let ms = start.elapsed().as_millis();
        match result {
            Ok((decoder, count)) => {
                let ok = count.abs_diff(expect) <= tolerance;
                failed += !ok as u32;
                println!("{decoder:<53} {count:>8} {ms:>8}  {}", if ok { "PASS" } else { "FAIL" });
            }
            Err(e) if e.contains("no Media Foundation decoder") => {
                println!("{:<53} {:>8} {ms:>8}  SKIP (decoder not installed)", "-", "-");
            }
            Err(e) => {
                failed += 1;
                println!("{:<53} {:>8} {ms:>8}  FAIL: {e}", "-", "-");
            }
        }
    }
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
