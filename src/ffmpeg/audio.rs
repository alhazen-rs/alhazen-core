//! `AudioDecoder` that runs ffmpeg as a pure decoder: Matroska in, interleaved f32le out.

use std::collections::VecDeque;
use std::io::Read;
use std::process::ChildStdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender};

use super::locate::FfmpegInfo;
use super::mkv::{BlockWriter, header};
use super::process::FfmpegProcess;
use crate::decode::{AudioBuffer, AudioDecoder, DelayTrim};
use crate::demux::{Codec, Packet, StreamInfo};
use crate::{Error, Result};

const STALL: Duration = Duration::from_secs(10);
/// Sample frames per output buffer.
const CHUNK_FRAMES: usize = 1024;

pub struct FfmpegAudioDecoder {
    info: Arc<FfmpegInfo>,
    stream: StreamInfo,
    rate: u32,
    channels: u16,
    process: Option<FfmpegProcess<Vec<f32>>>,
    blocks: BlockWriter,
    /// Time of the first output sample of the current process, and frames emitted since.
    start: Option<Duration>,
    emitted: u64,
    ready: VecDeque<Vec<f32>>,
    eof: bool,
    /// Start-up padding. Every ffmpeg trims Opus's pre-skip itself (even with no CodecDelay);
    /// for other codecs ffmpeg 6 ignores Matroska's CodecDelay and newer versions honour it, so
    /// it is not declared to ffmpeg and is trimmed here.
    trim: DelayTrim,
}

impl FfmpegAudioDecoder {
    pub fn new(info: Arc<FfmpegInfo>, stream: &StreamInfo) -> Result<Self> {
        header(stream).ok_or(Error::Unsupported("codec cannot be passed to ffmpeg"))?;
        let rate = if stream.sample_rate > 0 { stream.sample_rate } else { 48_000 };
        let channels = if stream.channels > 0 { stream.channels } else { 2 };
        let (rate, channels) = match (&stream.codec, stream.extradata.as_deref()) {
            (Codec::Aac, Some(asc)) => aac_output(asc, rate, channels),
            _ => (rate, channels),
        };
        let ffmpeg_trims = stream.codec == Codec::Opus;
        let (declared, trimmed) =
            if ffmpeg_trims { (stream.codec_delay, Duration::ZERO) } else { (Duration::ZERO, stream.codec_delay) };
        Ok(Self {
            info,
            stream: StreamInfo { codec_delay: declared, ..stream.clone() },
            rate,
            channels,
            process: None,
            blocks: BlockWriter::default(),
            start: None,
            emitted: 0,
            ready: VecDeque::new(),
            eof: false,
            trim: DelayTrim::new(trimmed).with_end(stream.end_trim),
        })
    }

    fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = ["-hide_banner", "-nostats", "-loglevel", "error"].map(String::from).to_vec();
        a.extend(["-probesize", "32768", "-analyzeduration", "1"].map(String::from));
        a.extend(["-f", "matroska", "-i", "pipe:0", "-map", "0:a:0", "-vn", "-sn"].map(String::from));
        a.extend(["-ac".into(), self.channels.to_string(), "-ar".into(), self.rate.to_string()]);
        a.extend(["-f", "f32le", "pipe:1"].map(String::from));
        a
    }

    /// Times ffmpeg's output from the first packet of the run, then trims the start-up padding;
    /// `None` when all of it was padding.
    fn buffer(&mut self, samples: Vec<f32>) -> Option<AudioBuffer> {
        let start = self.start.unwrap_or_default();
        let pts = start + Duration::from_secs_f64(self.emitted as f64 / self.rate as f64);
        self.emitted += (samples.len() / self.channels as usize) as u64;
        self.trim.apply(samples, self.channels, self.rate, pts)
    }

    /// The next block of ffmpeg's output, waiting for it only after end of stream.
    fn receive_raw(&mut self) -> Result<Option<Vec<f32>>> {
        if let Some(s) = self.ready.pop_front() {
            return Ok(Some(s));
        }
        let eof = self.eof;
        let Some(p) = self.process.as_mut() else { return Ok(None) };
        if let Some(s) = p.try_recv() {
            return Ok(Some(s));
        }
        if !eof {
            if p.is_drained() {
                let err = p.failure("ffmpeg exited during decoding");
                self.process = None;
                return Err(err);
            }
            return Ok(None);
        }
        let started = Instant::now();
        loop {
            match p.recv_timeout(Duration::from_millis(50)) {
                Ok(s) => return Ok(Some(s)),
                Err(RecvTimeoutError::Disconnected) => {
                    let err = p.exit_failed().then(|| p.failure("ffmpeg failed"));
                    self.process = None;
                    return err.map_or(Ok(None), Err);
                }
                Err(RecvTimeoutError::Timeout) if started.elapsed() > STALL => {
                    let err = p.failure("ffmpeg stalled");
                    self.process = None;
                    return Err(err);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

impl AudioDecoder for FfmpegAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.eof = false;
        self.trim.on_packet(packet.pts);
        if self.process.is_none() {
            let head = header(&self.stream).expect("checked in new");
            let frame_bytes = self.channels as usize * 4;
            let p = FfmpegProcess::spawn(&self.info.path, &self.args(), head, 16, move |out, tx| {
                read_f32(out, tx, frame_bytes)
            })?;
            self.process = Some(p);
            self.blocks = BlockWriter::default();
            self.start = Some(packet.pts);
            self.emitted = 0;
        }
        let bytes = self.blocks.block(packet.pts, packet.keyframe, &packet.data);
        let mut ready = std::mem::take(&mut self.ready);
        let result = self.process.as_mut().unwrap().write(bytes, &mut ready);
        self.ready = ready;
        result
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        loop {
            match self.receive_raw()? {
                Some(s) => match self.buffer(s) {
                    Some(b) => return Ok(Some(b)),
                    None => continue, // padding only
                },
                None => return Ok(None),
            }
        }
    }

    fn send_eof(&mut self) {
        self.eof = true;
        if let Some(p) = self.process.as_mut() {
            p.close_input();
        }
    }

    fn flush(&mut self) {
        self.process = None;
        self.blocks = BlockWriter::default();
        self.ready.clear();
        self.start = None;
        self.emitted = 0;
        self.eof = false;
        self.trim.reset();
    }
}

/// AAC's decoded rate and channels from its AudioSpecificConfig (ISO 14496-3 1.6.2.1), which may
/// differ from the container's: SBR (HE-AAC) doubles the core rate, PS (HE-AACv2) makes mono
/// stereo. Implicitly signalled SBR/PS is only found in the stream, so a core rate of 24 kHz or
/// less is assumed to carry them (as decoders do); a stream without them is merely upsampled.
fn aac_output(asc: &[u8], rate: u32, channels: u16) -> (u32, u16) {
    const RATES: [u32; 13] = [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];
    let mut r = Bits { data: asc, pos: 0 };
    let parsed = (|| {
        let aot = r.object_type()?;
        let core = r.frequency(&RATES)?;
        let config = r.read(4)?;
        Some(match aot {
            5 | 29 => (r.frequency(&RATES)?, if aot == 29 && config == 1 { 2 } else { channels }),
            _ if core <= 24_000 => (core * 2, if config == 1 { 2 } else { channels }),
            _ => (rate, channels),
        })
    })();
    parsed.unwrap_or((rate, channels))
}

/// MSB-first bit reader over an AudioSpecificConfig.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| {
            let bit = (self.data.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Some(v << 1 | bit as u32)
        })
    }

    fn object_type(&mut self) -> Option<u32> {
        let t = self.read(5)?;
        if t == 31 { Some(32 + self.read(6)?) } else { Some(t) }
    }

    fn frequency(&mut self, rates: &[u32]) -> Option<u32> {
        let i = self.read(4)?;
        if i == 15 { self.read(24) } else { rates.get(i as usize).copied() }
    }
}

/// Cuts stdout into buffers of whole sample frames (at least 256 frames each, except at the end).
fn read_f32(mut out: ChildStdout, tx: Sender<Vec<f32>>, frame_bytes: usize) {
    let mut buf = vec![0u8; CHUNK_FRAMES * frame_bytes];
    let mut ended = false;
    while !ended {
        let mut filled = 0;
        while filled < buf.len() && !(filled % frame_bytes == 0 && filled >= 256 * frame_bytes) {
            match out.read(&mut buf[filled..]) {
                Ok(0) | Err(_) => {
                    ended = true;
                    break;
                }
                Ok(n) => filled += n,
            }
        }
        let whole = filled - filled % frame_bytes;
        if whole > 0 {
            let samples = buf[..whole].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            if tx.send(samples).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod aac_tests {
    use super::aac_output;

    #[test]
    fn aac_lc_keeps_the_container_rate() {
        assert_eq!(aac_output(&[0x12, 0x10], 44_100, 2), (44_100, 2)); // AOT 2, 44.1 kHz, stereo
    }

    #[test]
    fn explicit_sbr_uses_the_extension_rate() {
        // AOT 5, core 24 kHz, stereo, extension 48 kHz, then AOT 2.
        assert_eq!(aac_output(&[0x2B, 0x11, 0x88, 0x00], 24_000, 2), (48_000, 2));
    }

    #[test]
    fn explicit_ps_is_stereo() {
        // AOT 29, core 24 kHz, mono, extension 48 kHz, then AOT 2.
        assert_eq!(aac_output(&[0xEB, 0x09, 0x88, 0x00], 24_000, 1), (48_000, 2));
    }

    #[test]
    fn low_core_rates_assume_implicit_sbr() {
        assert_eq!(aac_output(&[0x13, 0x88], 22_050, 1), (44_100, 2)); // AOT 2, 22.05 kHz, mono
    }

    #[test]
    fn garbage_keeps_the_container_values() {
        assert_eq!(aac_output(&[], 44_100, 2), (44_100, 2));
    }
}
