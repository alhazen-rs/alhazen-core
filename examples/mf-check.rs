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

    if std::env::args().nth(1).as_deref() == Some("--native-type") {
        native_types(&std::env::args().nth(2).expect("usage: mf-check --native-type <file>"));
        return;
    }
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

/// `mf-check --native-type <file>`: the media types Windows' own demuxer (Source Reader) reports
/// for each stream, every attribute included. Shows what Windows' decoders expect as input.
#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn native_types(file: &str) {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::{GUID, HSTRING};

    let names: &[(GUID, &str)] = &[
        (MF_MT_MAJOR_TYPE, "MAJOR_TYPE"),
        (MF_MT_SUBTYPE, "SUBTYPE"),
        (MF_MT_AUDIO_SAMPLES_PER_SECOND, "AUDIO_SAMPLES_PER_SECOND"),
        (MF_MT_AUDIO_NUM_CHANNELS, "AUDIO_NUM_CHANNELS"),
        (MF_MT_AUDIO_BITS_PER_SAMPLE, "AUDIO_BITS_PER_SAMPLE"),
        (MF_MT_AUDIO_BLOCK_ALIGNMENT, "AUDIO_BLOCK_ALIGNMENT"),
        (MF_MT_AUDIO_AVG_BYTES_PER_SECOND, "AUDIO_AVG_BYTES_PER_SECOND"),
        (MF_MT_AUDIO_CHANNEL_MASK, "AUDIO_CHANNEL_MASK"),
        (MF_MT_AUDIO_VALID_BITS_PER_SAMPLE, "AUDIO_VALID_BITS_PER_SAMPLE"),
        (MF_MT_AUDIO_SAMPLES_PER_BLOCK, "AUDIO_SAMPLES_PER_BLOCK"),
        (MF_MT_USER_DATA, "USER_DATA"),
        (MF_MT_ALL_SAMPLES_INDEPENDENT, "ALL_SAMPLES_INDEPENDENT"),
        (MF_MT_FIXED_SIZE_SAMPLES, "FIXED_SIZE_SAMPLES"),
        (MF_MT_SAMPLE_SIZE, "SAMPLE_SIZE"),
        (MF_MT_COMPRESSED, "COMPRESSED"),
        (MF_MT_AUDIO_PREFER_WAVEFORMATEX, "AUDIO_PREFER_WAVEFORMATEX"),
    ];
    let path = std::path::absolute(file).expect("path");
    // SAFETY: COM/MF calls on objects owned here; blob buffers are sized from GetBlobSize.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None).expect("Source Reader");
        for stream in 0u32.. {
            let Ok(t) = reader.GetNativeMediaType(stream, 0) else { break };
            println!("stream {stream}:");
            for i in 0..t.GetCount().unwrap_or(0) {
                let mut key = GUID::zeroed();
                if t.GetItemByIndex(i, &mut key, None).is_err() {
                    continue;
                }
                let name = names.iter().find(|(g, _)| *g == key).map(|(_, n)| n.to_string()).unwrap_or(format!("{key:?}"));
                let value = match t.GetItemType(&key) {
                    Ok(MF_ATTRIBUTE_UINT32) => t.GetUINT32(&key).map(|v| v.to_string()).unwrap_or_default(),
                    Ok(MF_ATTRIBUTE_UINT64) => t.GetUINT64(&key).map(|v| v.to_string()).unwrap_or_default(),
                    Ok(MF_ATTRIBUTE_GUID) => t.GetGUID(&key).map(|v| format!("{v:?}")).unwrap_or_default(),
                    Ok(MF_ATTRIBUTE_BLOB) => {
                        let n = t.GetBlobSize(&key).unwrap_or(0) as usize;
                        let mut b = vec![0u8; n];
                        let _ = t.GetBlob(&key, &mut b, None);
                        format!("{n} bytes: {}", b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" "))
                    }
                    Ok(other) => format!("(type {})", other.0),
                    Err(e) => format!("({e})"),
                };
                println!("  {name} = {value}");
            }
        }
    }
}
