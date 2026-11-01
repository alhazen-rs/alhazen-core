//! `mf-check <fixtures-dir>`: decodes the test fixtures through Windows' own decoders (Media
//! Foundation) and reports, per file, which decoder ran, GPU or software, and PASS/FAIL; then
//! compares the D3D11 video processor's BGRA with the CPU path's. Built in CI and run from
//! `prebuilt/` on a Windows machine without a Rust toolchain. `--dump DIR` after the fixtures
//! directory also saves both pictures of each comparison there.

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
    use alhazen_core::backend::Registry;
    use alhazen_core::decode::{AudioDecoder, DecodedFrame, VideoDecoder};
    use alhazen_core::demux::StreamKind;
    use alhazen_core::hw::select::HwCodec;
    use alhazen_core::mf::{MfAudioDecoder, MfVideoDecoder};
    use alhazen_core::Source;

    if std::env::args().nth(1).as_deref() == Some("--native-type") {
        native_types(&std::env::args().nth(2).expect("usage: mf-check --native-type <file>"));
        return;
    }
    let dir = std::env::args().nth(1).unwrap_or_else(|| "tests/fixtures".into());
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
        ("alac.m4a", StreamKind::Audio, 24_000, 4096),
    ];
    let mut failed = 0u32;
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
            let format = alhazen_core::demux::probe(src.as_mut()).map_err(|e| e.to_string())?.ok_or("not a media file")?;
            let mut d = Registry::empty_with_native().open_demuxer(&source, format, src, None).map_err(|e| e.to_string())?;
            let s = d.streams().iter().find(|s| s.kind == kind).cloned().ok_or("no such track")?;
            let codec = HwCodec::of(&s).ok_or("not a Media Foundation codec")?;
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
    let dump = std::env::args().skip_while(|a| a != "--dump").nth(1);
    failed += processor_checks(&dir, dump.as_deref());
    std::process::exit(if failed == 0 { 0 } else { 1 });
}

/// The D3D11 video processor against the CPU path, as in `tests/media_foundation.rs`. Returns
/// how many checks failed; a machine without a video processor skips them.
#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn processor_checks(dir: &str, dump: Option<&str>) -> u32 {
    use std::time::Duration;
    // (file, largest output size, least PSNR against the CPU path in dB)
    let cases = [("h264_aac.mp4", None, 45.0), ("h264_aac.mp4", Some((160, 90)), 35.0), ("hdr_pq.mkv", None, 30.0)];
    let mut failed = 0;
    println!(
        "\nD3D11 video processor against the CPU path\n{:<18} {:<10} {:>8} {:>10}  {:>8} {:<16} {:<8} result",
        "file", "size", "dB", "luma", "luma dB", "mean diff BGR", "alpha"
    );
    for (file, hint, min_db) in cases {
        let path = format!("{dir}/{file}");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(processor_and_cpu(&path, hint));
        });
        // Printed before converting so a driver that hangs or crashes the process is named.
        print!("{file:<18} ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        match rx.recv_timeout(Duration::from_secs(20)) {
            Err(_) => {
                failed += 1;
                println!("FAIL: hung or crashed");
            }
            Ok(Err(e)) => {
                failed += 1;
                println!("FAIL: {e}");
            }
            Ok(Ok(None)) => println!("SKIP (no D3D11 video processor here)"),
            Ok(Ok(Some((bgra, size, y)))) => {
                let want = hint.and_then(|h| alhazen_core::scale::fit_within(y.width, y.height, h)).unwrap_or((y.width, y.height));
                let reference = cpu_reference(&y, size);
                let db = psnr(&bgra, &reference);
                if let Some(dump) = dump {
                    let name = format!("{dump}/{}-{}x{}", file.split('.').next().unwrap_or(file), size.0, size.1);
                    let saved = std::fs::create_dir_all(dump)
                        .and_then(|()| write_ppm(&format!("{name}-gpu.ppm"), &bgra, size))
                        .and_then(|()| write_ppm(&format!("{name}-cpu.ppm"), &reference, size));
                    if let Err(e) = saved {
                        println!("(could not save {name}: {e})");
                    }
                }
                // Drivers tone map their own way: HDR must also reach black and white.
                let luma = |p: &[u8]| (p[2] as u32 * 2126 + p[1] as u32 * 7152 + p[0] as u32 * 722) / 10000;
                let (lo, hi) = bgra.chunks(4).map(luma).fold((255, 0), |(lo, hi), l| (lo.min(l), hi.max(l)));
                let range_ok = !file.starts_with("hdr") || (lo <= 8 && hi >= 240);
                let ok = size == want && db >= min_db && range_ok;
                failed += !ok as u32;
                let lumas = |p: &[u8]| p.chunks(4).map(|px| luma(px) as u8).collect::<Vec<u8>>();
                let mean_diff = |c: usize| {
                    bgra.chunks(4).zip(reference.chunks(4)).map(|(a, b)| a[c] as f64 - b[c] as f64).sum::<f64>() / (bgra.len() / 4) as f64
                };
                let alpha = bgra.chunks(4).map(|px| px[3]).fold((255, 0), |(lo, hi), a| (a.min(lo), a.max(hi)));
                println!(
                    "{:<10} {db:>8.1} {:>10}  {:>8.1} {:<16} {:<8} {}",
                    format!("{}x{}", size.0, size.1),
                    format!("{lo}..{hi}"),
                    psnr(&lumas(&bgra), &lumas(&reference)),
                    format!("{:+.1} {:+.1} {:+.1}", mean_diff(0), mean_diff(1), mean_diff(2)),
                    format!("{}..{}", alpha.0, alpha.1),
                    if ok { "PASS" } else { "FAIL" }
                );
            }
        }
    }
    failed
}

/// A frame from the video processor (BGRA and its size) and the same one as YUV.
#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
type Converted = (Vec<u8>, (u32, u32), alhazen_core::decode::YuvFrame);

/// The first frame of `path` through Media Foundation, once converted by the video processor
/// (with `hint`) and once as YUV for the CPU path; `None` when that can't run here.
#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn processor_and_cpu(path: &str, hint: Option<(u32, u32)>) -> Result<Option<Converted>, String> {
    use alhazen_core::backend::Registry;
    use alhazen_core::decode::{DecodedFrame, VideoDecoder};
    use alhazen_core::demux::StreamKind;
    use alhazen_core::hw::select::HwCodec;
    use alhazen_core::mf::MfVideoDecoder;
    use alhazen_core::Source;

    let source = Source::parse(path).map_err(|e| e.to_string())?;
    let mut src = source.open().map_err(|e| e.to_string())?;
    let format = alhazen_core::demux::probe(src.as_mut()).map_err(|e| e.to_string())?.ok_or("not a media file")?;
    let mut d = Registry::empty_with_native().open_demuxer(&source, format, src, None).map_err(|e| e.to_string())?;
    let s = d.streams().iter().find(|s| s.kind == StreamKind::Video).cloned().ok_or("no video track")?;
    let codec = HwCodec::of(&s).ok_or("not a Media Foundation codec")?;
    let unavailable = |e: &str| e.contains("no Media Foundation decoder");
    let mut gpu = match MfVideoDecoder::new(codec, &s, true) {
        Ok(dec) => dec.with_ten_bit(true).with_gpu_bgra(true),
        Err(e) if unavailable(&e.to_string()) => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut cpu = MfVideoDecoder::new(codec, &s, true).map_err(|e| e.to_string())?.with_ten_bit(true);
    gpu.set_output_hint(hint);
    let (mut g, mut c) = (None, None);
    while g.is_none() || c.is_none() {
        let Some(p) = d.next_packet().map_err(|e| e.to_string())? else { break };
        if p.stream != s.id {
            continue;
        }
        if let Err(e) = gpu.send_packet(&p).and_then(|()| cpu.send_packet(&p)) {
            return if unavailable(&e.to_string()) { Ok(None) } else { Err(e.to_string()) };
        }
        g = g.or(gpu.receive_frame().map_err(|e| e.to_string())?);
        c = c.or(cpu.receive_frame().map_err(|e| e.to_string())?);
    }
    let Some(DecodedFrame::Yuv(mut y)) = c else { return Err("the CPU path gave no YUV frame".into()) };
    y.color.apply_stream(&alhazen_core::color::StreamColor::of(&s));
    match g {
        Some(DecodedFrame::Bgra { width, height, bgra, .. }) => Ok(Some((bgra.to_vec(), (width, height), y))),
        _ if gpu.gpu_fallback() || gpu.description().is_some_and(|(_, on_gpu)| !on_gpu) => Ok(None),
        _ => Err("the GPU path gave no BGRA frame".into()),
    }
}

/// The CPU path's full-size BGRA, scaled to `size` in RGB (see `tests/nvdec.rs`).
#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn cpu_reference(y: &alhazen_core::decode::YuvFrame, size: (u32, u32)) -> Vec<u8> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let mut full = Vec::new();
    alhazen_core::convert::frame_to_bgra(y, &mut full).unwrap();
    if size == (y.width, y.height) {
        return full;
    }
    let src = ImageRef::new(y.width, y.height, &full, PixelType::U8x4).unwrap();
    let mut dst = Image::new(size.0, size.1, PixelType::U8x4);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    Resizer::new().resize(&src, &mut dst, &options).unwrap();
    dst.into_vec()
}

/// A BGRA picture as a binary PPM (RGB, no alpha).
#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn write_ppm(path: &str, bgra: &[u8], (width, height): (u32, u32)) -> std::io::Result<()> {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    out.extend(bgra.chunks(4).flat_map(|p| [p[2], p[1], p[0]]));
    std::fs::write(path, out)
}

#[cfg(all(windows, feature = "media-foundation", feature = "native"))]
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
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
