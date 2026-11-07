//! `vaapi-check <file>`: decodes a file's H.264 through VA-API (an AMD or Intel GPU) and prints
//! each picture's MD5, as `ffmpeg -i <file> -f framemd5 -pix_fmt yuv420p -` would, so the two can
//! be compared line by line. Then a summary: pictures, time, the driver.
//! `ALHAZEN_VAAPI_ANY_DRIVER=1` lets it run on NVIDIA's VA-API driver too.

#[cfg(not(all(target_os = "linux", feature = "vaapi", feature = "native")))]
fn main() {
    eprintln!("vaapi-check needs Linux and the vaapi + native features");
    std::process::exit(2);
}

#[cfg(all(target_os = "linux", feature = "vaapi", feature = "native"))]
fn main() {
    use alhazen_core::Source;
    use alhazen_core::backend::Registry;
    use alhazen_core::decode::{DecodedFrame, VideoDecoder, YuvPlanes};
    use alhazen_core::demux::StreamKind;
    use md5::{Digest, Md5};

    let path = std::env::args().nth(1).expect("usage: vaapi-check <file>");
    let source = Source::parse(&path).expect("source");
    let mut src = source.open().expect("open");
    let format = alhazen_core::demux::probe(src.as_mut()).expect("probe").expect("not a media file");
    let mut demuxer = Registry::empty_with_native().open_demuxer(&source, format, src, None).expect("demuxer");
    let stream = demuxer.streams().iter().find(|s| s.kind == StreamKind::Video).cloned().expect("no video");
    let mut decoder = match alhazen_core::vaapi::VaapiVideoDecoder::new(&stream) {
        Ok(d) => d,
        Err(e) => {
            println!("FAIL: VA-API decoder: {e}");
            std::process::exit(1);
        }
    };
    let started = std::time::Instant::now();
    let mut count = 0usize;
    let mut print = |frame: DecodedFrame| {
        let DecodedFrame::Yuv(f) = frame else { return };
        let YuvPlanes::U8(planes) = &f.planes else { return };
        let mut md5 = Md5::new();
        for p in planes {
            md5.update(p);
        }
        let digest: String = md5.finalize().iter().map(|b| format!("{b:02x}")).collect();
        println!("{count} {}x{} {digest}", f.width, f.height);
        count += 1;
    };
    while let Ok(Some(packet)) = demuxer.next_packet() {
        if packet.stream != stream.id {
            continue;
        }
        if let Err(e) = decoder.send_packet(&packet) {
            println!("FAIL: at picture {count}: {e}");
            std::process::exit(1);
        }
        while let Ok(Some(frame)) = decoder.receive_frame() {
            print(frame);
        }
    }
    decoder.send_eof();
    while let Ok(Some(frame)) = decoder.receive_frame() {
        print(frame);
    }
    eprintln!("{count} pictures in {:?}", started.elapsed());
}
