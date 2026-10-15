//! A GPU conversion that can't start (the kernel won't load) falls back to the CPU path.
//! Its own test binary: it sets a process-wide switch.
#![cfg(all(target_os = "linux", feature = "nvdec", feature = "native"))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use alhazen_core::audio::AudioOutputConfig;
use alhazen_core::clock::MockClock;
use alhazen_core::{FrameFormat, Player, PlayerConfig, Source, VideoFrame};

#[test]
fn ptx_load_failure_falls_back_to_cpu() {
    if !alhazen_core::nvdec::available() {
        return eprintln!("skipped: no NVDEC on this machine");
    }
    // SAFETY: set before any thread of this test binary reads the environment.
    unsafe { std::env::set_var("ALHAZEN_TEST_BREAK_PTX", "1") };
    let config = PlayerConfig {
        clock: Some(Arc::new(MockClock::new())),
        audio_output: AudioOutputConfig::Disabled,
        backend_order: Some(vec!["nvdec"]),
        frame_formats: vec![FrameFormat::Bgra],
        ..Default::default()
    };
    let path = format!("{}/tests/fixtures/h264_aac.mp4", env!("CARGO_MANIFEST_DIR"));
    let player = Player::open(Source::parse(&path).unwrap(), config).unwrap();
    let start = Instant::now();
    let frame = loop {
        if let Some(f) = player.current_frame() {
            break f;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "no frame");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(matches!(frame, VideoFrame::Bgra { .. }));
    assert_eq!(player.stats().video_backend, Some("nvdec"));
    assert_eq!(player.stats().conversion, "cpu (fallback)");
}

