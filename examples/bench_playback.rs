//! Plays a file headless at real-time speed and reports what it costs: process CPU time per
//! second of video, frames shown, frames dropped, the video backend and conversion path.
//!
//! ```text
//! cargo run --release --example bench_playback -- <file> [seconds] [--display WxH] [--cpu]
//! ```
//! `--display` asks for frames no larger than that (as a window of that size would);
//! `--cpu` forces the CPU conversion path (the decoders hand out YUV, converted on the CPU).

use std::time::{Duration, Instant};

use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::{FrameFormat, Player, PlayerConfig, PlayerState, Source};

/// User + system CPU time of this process so far.
#[cfg(unix)]
fn cpu_time() -> Duration {
    // SAFETY: getrusage on our own process into a zeroed struct.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

#[cfg(windows)]
fn cpu_time() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: our own process handle and four valid out-pointers.
    let _ = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Duration::from_nanos((ticks(k) + ticks(u)) * 100)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().expect("usage: bench_playback <file> [seconds] [--display WxH] [--cpu]");
    let seconds: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
    let display = args.iter().position(|a| a == "--display").and_then(|i| args.get(i + 1)).and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let cpu = args.iter().any(|a| a == "--cpu");
    let config = PlayerConfig {
        audio_output: AudioOutputConfig::Disabled,
        max_output_size: display,
        // `Yuv` makes the decoders hand out planes; the benchmark converts them like the CPU path.
        frame_formats: if cpu { vec![FrameFormat::Yuv] } else { vec![FrameFormat::Bgra] },
        ..Default::default()
    };
    let player = Player::open(Source::parse(path).expect("source"), config).expect("open");
    let start = Instant::now();
    while player.current_frame().is_none() {
        assert!(start.elapsed() < Duration::from_secs(20), "no first frame");
        std::thread::sleep(Duration::from_millis(5));
    }
    player.play();
    let (wall0, cpu0) = (Instant::now(), cpu_time());
    let dropped0 = player.stats().frames_dropped;
    let (mut shown, mut last, mut out) = (0u64, None, Vec::new());
    while wall0.elapsed() < Duration::from_secs(seconds) && player.state() != PlayerState::Ended {
        if let Some(f) = player.current_frame()
            && last != Some(f.pts())
        {
            last = Some(f.pts());
            shown += 1;
            if cpu && let alhazen_core::VideoFrame::Yuv(y) = &f {
                // What the player's own CPU path would do with this frame.
                out.clear();
                let _ = alhazen_core::convert::frame_to_bgra(y, &mut out);
            }
        }
        std::thread::sleep(Duration::from_millis(2)); // a 500 Hz "display"
    }
    let wall = wall0.elapsed().as_secs_f64();
    let cpu_s = (cpu_time() - cpu0).as_secs_f64();
    let s = player.stats();
    println!(
        "{path}\n  {:.2} CPU-seconds per second of video ({:.0}% of one core) | {:.1} fps shown | {} dropped | {} via {} | {:?}",
        cpu_s / wall,
        100.0 * cpu_s / wall,
        shown as f64 / wall,
        s.frames_dropped - dropped0,
        s.video_backend.unwrap_or("-"),
        if cpu { "cpu (forced)" } else { s.conversion },
        s.hdr,
    );
}
