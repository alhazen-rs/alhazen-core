//! Audio decoding: packets -> interleaved f32 sample buffers.

use std::time::Duration;

use crate::Result;
use crate::demux::Packet;

/// Decoded audio: interleaved f32 samples in [-1, 1].
#[derive(Clone, Debug, PartialEq)]
pub struct AudioBuffer {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
    pub pts: Duration,
}

impl AudioBuffer {
    /// Number of sample frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    pub fn duration(&self) -> Duration {
        Duration::from_nanos(self.frames() as u64 * 1_000_000_000 / self.rate.max(1) as u64)
    }
}

pub trait AudioDecoder: Send {
    fn send_packet(&mut self, packet: &Packet) -> Result<()>;
    /// `Ok(None)` means the decoder needs more input.
    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>>;
    /// Drops all buffered state; called on seek.
    fn flush(&mut self);
}

#[cfg(all(test, feature = "native"))]
pub(crate) mod tests {
    use super::*;
    use crate::demux::{Demuxer, MatroskaDemuxer, Mp4Demuxer, StreamInfo, StreamKind};
    use crate::source::FileSource;

    /// Decodes every audio packet of `path` with the decoder built by `make`.
    pub(crate) fn decode_file(
        path: &str,
        make: impl FnOnce(&StreamInfo) -> Result<Box<dyn AudioDecoder>>,
    ) -> (StreamInfo, Vec<AudioBuffer>) {
        let src = Box::new(FileSource::open(path).unwrap());
        let mut demuxer: Box<dyn Demuxer> = if path.ends_with(".webm") {
            Box::new(MatroskaDemuxer::open(src).unwrap())
        } else {
            Box::new(Mp4Demuxer::open(src).unwrap())
        };
        let info = demuxer.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap().clone();
        let mut dec = make(&info).unwrap();
        let mut out = vec![];
        while let Some(p) = demuxer.next_packet().unwrap() {
            if p.stream != info.id {
                continue;
            }
            dec.send_packet(&p).unwrap();
            while let Some(b) = dec.receive_samples().unwrap() {
                out.push(b);
            }
        }
        (info, out)
    }

    pub(crate) fn total_frames(bufs: &[AudioBuffer]) -> usize {
        bufs.iter().map(|b| b.frames()).sum()
    }

    /// Sign changes of channel 0, ignoring near-silent samples (codec priming/padding noise).
    pub(crate) fn zero_crossings(bufs: &[AudioBuffer]) -> usize {
        let mut last_positive = None;
        let mut n = 0;
        for b in bufs {
            for s in b.samples.chunks(b.channels as usize).map(|f| f[0]) {
                if s.abs() < 0.05 {
                    continue;
                }
                let positive = s > 0.0;
                if last_positive.is_some_and(|p| p != positive) {
                    n += 1;
                }
                last_positive = Some(positive);
            }
        }
        n
    }

    #[test]
    fn opus_decodes_two_seconds_of_440hz() {
        let (_, bufs) = decode_file("tests/fixtures/opus_only.webm", |s| {
            Ok(Box::new(crate::decode::OpusAudioDecoder::new(s)?))
        });
        assert!(bufs.iter().all(|b| b.rate == 48_000 && b.channels == 1));
        let frames = total_frames(&bufs);
        assert!((95_000..=97_000).contains(&frames), "frames = {frames}");
        let zc = zero_crossings(&bufs);
        assert!((1_700..=1_820).contains(&zc), "440 Hz over 2 s ≈ 1760 crossings, got {zc}");
        assert!(bufs.windows(2).all(|w| w[0].pts < w[1].pts));
    }

    #[test]
    fn opus_surround_decodes_six_channels() {
        let (_, bufs) = decode_file("tests/fixtures/opus_51.webm", |s| {
            Ok(Box::new(crate::decode::OpusAudioDecoder::new(s)?))
        });
        assert!(bufs.iter().all(|b| b.channels == 6 && b.rate == 48_000));
        let frames = total_frames(&bufs);
        assert!((95_000..=97_000).contains(&frames), "frames = {frames}");
        assert!((1_700..=1_820).contains(&zero_crossings(&bufs)));
    }

    #[test]
    fn opus_surround_comes_out_in_wave_channel_order() {
        // Tone on the centre only; Opus mapping family 1 uses Vorbis order (FL FC FR …),
        // which must come out as WAVE order (FL FR FC LFE BL BR).
        let (_, bufs) = decode_file("tests/fixtures/opus_51_center.webm", |s| {
            Ok(Box::new(crate::decode::OpusAudioDecoder::new(s)?))
        });
        let mut energy = [0f32; 6];
        for b in &bufs {
            for f in b.samples.chunks(6) {
                for (e, s) in energy.iter_mut().zip(f) {
                    *e += s * s;
                }
            }
        }
        let loudest = energy.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(loudest, 2, "centre must be WAVE channel 2, energies {energy:?}");
        let others: f32 = energy.iter().enumerate().filter(|(i, _)| *i != 2).map(|(_, e)| e).sum();
        assert!(others < energy[2] * 0.05, "tone leaked into other channels: {energy:?}");
    }

    #[test]
    fn vorbis_decodes_two_seconds() {
        let (_, bufs) = decode_file("tests/fixtures/av1_vorbis.webm", |s| {
            Ok(Box::new(crate::decode::VorbisAudioDecoder::new(s)?))
        });
        assert!(bufs.iter().all(|b| b.rate == 44_100 && b.channels == 1));
        let frames = total_frames(&bufs);
        assert!((86_000..=90_400).contains(&frames), "frames = {frames}");
        let zc = zero_crossings(&bufs);
        assert!((1_700..=1_820).contains(&zc), "got {zc}");
    }

    #[test]
    fn vorbis_surround_comes_out_in_wave_channel_order() {
        // Tone on the centre channel only. Vorbis stores 5.1 as FL, FC, FR, BL, BR, LFE;
        // decoders must hand out WAVE order (FL, FR, FC, LFE, BL, BR) for the mixer.
        let (_, bufs) = decode_file("tests/fixtures/vorbis_51_center.webm", |s| {
            Ok(Box::new(crate::decode::VorbisAudioDecoder::new(s)?))
        });
        let mut energy = [0f32; 6];
        for b in &bufs {
            assert_eq!(b.channels, 6);
            for f in b.samples.chunks(6) {
                for (e, s) in energy.iter_mut().zip(f) {
                    *e += s * s;
                }
            }
        }
        let loudest = energy.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(loudest, 2, "centre must be WAVE channel 2, energies {energy:?}");
    }

    #[cfg(feature = "native-aac")]
    #[test]
    fn aac_decodes_two_seconds_from_mp4() {
        let (info, bufs) = decode_file("tests/fixtures/aac_only.m4a", |s| {
            Ok(Box::new(crate::decode::AacAudioDecoder::new(s)?))
        });
        assert_eq!(info.extradata.as_deref(), Some(&[0x12, 0x08, 0x56, 0xE5, 0x00][..]), "AAC-LC, 44.1 kHz, mono");
        assert!(bufs.iter().all(|b| b.rate == 44_100 && b.channels == 1));
        let frames = total_frames(&bufs);
        assert!((86_000..=92_500).contains(&frames), "frames = {frames}");
        let zc = zero_crossings(&bufs);
        assert!((1_700..=1_830).contains(&zc), "got {zc}");
    }

    /// Decoded buffers must carry the same timestamps as ffmpeg's decoder (the reference) gives
    /// its frames: Matroska block time minus CodecDelay. Values from
    /// `ffprobe -select_streams a:0 -show_entries frame=pts_time,nb_samples` on the fixtures;
    /// Matroska timestamps have 1 ms resolution, hence the 1 ms tolerance.
    fn assert_matches_reference(path: &str, make: impl FnOnce(&StreamInfo) -> Result<Box<dyn AudioDecoder>>, expected: &[(u64, usize)]) {
        let (_, bufs) = decode_file(path, make);
        for (b, &(ms, frames)) in bufs.iter().zip(expected) {
            let want = Duration::from_millis(ms);
            assert!(b.pts.abs_diff(want) <= Duration::from_millis(1), "{path}: buffer pts {:?}, reference {want:?}", b.pts);
            assert_eq!(b.frames(), frames, "{path}: buffer at {:?}", b.pts);
        }
    }

    #[test]
    fn opus_timestamps_match_the_reference_decoder() {
        let make = |s: &StreamInfo| -> Result<Box<dyn AudioDecoder>> { Ok(Box::new(crate::decode::OpusAudioDecoder::new(s)?)) };
        assert_matches_reference("tests/fixtures/chirp_opus.webm", make, &[(0, 648), (14, 960), (34, 960), (54, 960)]);
    }

    #[test]
    fn vorbis_timestamps_match_the_reference_decoder() {
        let make = |s: &StreamInfo| -> Result<Box<dyn AudioDecoder>> { Ok(Box::new(crate::decode::VorbisAudioDecoder::new(s)?)) };
        assert_matches_reference("tests/fixtures/chirp_vorbis.webm", make, &[(0, 576), (13, 1024), (36, 1024), (60, 1024)]);
    }

    #[test]
    fn only_mapping_family_1_is_reordered() {
        // Same 5.1 stream, header relabelled as mapping family 255 (application-defined order):
        // channels must come out exactly as stored, so the centre tone stays on channel 1.
        let (_, bufs) = decode_file("tests/fixtures/opus_51_center.webm", |s| {
            let mut s = s.clone();
            s.extradata.as_mut().unwrap()[18] = 255;
            Ok(Box::new(crate::decode::OpusAudioDecoder::new(&s)?))
        });
        let mut energy = [0f32; 6];
        for b in &bufs {
            for f in b.samples.chunks(6) {
                for (e, s) in energy.iter_mut().zip(f) {
                    *e += s * s;
                }
            }
        }
        let loudest = energy.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(loudest, 1, "family 255 must not be reordered, energies {energy:?}");
    }
}
