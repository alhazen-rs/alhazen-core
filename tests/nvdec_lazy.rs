//! NVDEC's CUDA context (which powers up the GPU, e.g. the discrete one on hybrid laptops) is
//! created only once a video stream needs it. Its own test binary: a fresh process.
#![cfg(all(target_os = "linux", feature = "nvdec", feature = "native"))]

use alhazen_core::audio::{AudioOutputConfig, NullOutput};
use alhazen_core::{Player, PlayerConfig, Source};

fn open(name: &str) -> Player {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let config = PlayerConfig { audio_output: AudioOutputConfig::Null(NullOutput::new(48_000, 2)), ..Default::default() };
    Player::open(Source::parse(&path).unwrap(), config).unwrap()
}

#[test]
fn the_cuda_context_waits_for_a_video() {
    if !alhazen_core::nvdec::available() {
        eprintln!("skipped: no NVDEC");
        return;
    }
    let _registry = alhazen_core::backend::Registry::with_defaults();
    let _audio = open("opus_only.webm");
    assert!(!alhazen_core::nvdec::context_created(), "no video yet: the GPU stays idle");
    let video = open("h264_aac.mp4");
    assert_eq!(video.stats().video_backend, Some("nvdec"));
    assert!(alhazen_core::nvdec::context_created());
}
