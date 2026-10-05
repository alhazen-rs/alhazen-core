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
use crate::decode::{AudioBuffer, AudioDecoder};
use crate::demux::{Packet, StreamInfo};
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
}

impl FfmpegAudioDecoder {
    pub fn new(info: Arc<FfmpegInfo>, stream: &StreamInfo) -> Result<Self> {
        header(stream).ok_or(Error::Unsupported("codec cannot be passed to ffmpeg"))?;
        Ok(Self {
            info,
            stream: stream.clone(),
            rate: if stream.sample_rate > 0 { stream.sample_rate } else { 48_000 },
            channels: if stream.channels > 0 { stream.channels } else { 2 },
            process: None,
            blocks: BlockWriter::default(),
            start: None,
            emitted: 0,
            ready: VecDeque::new(),
            eof: false,
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

    fn buffer(&mut self, samples: Vec<f32>) -> AudioBuffer {
        // ffmpeg drops the stream's CodecDelay (we declare it) from the start of every run.
        let start = self.start.unwrap_or_default() + self.stream.codec_delay;
        let pts = start + Duration::from_secs_f64(self.emitted as f64 / self.rate as f64);
        self.emitted += (samples.len() / self.channels as usize) as u64;
        AudioBuffer { rate: self.rate, channels: self.channels, samples, pts }
    }
}

impl AudioDecoder for FfmpegAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.eof = false;
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
        if let Some(s) = self.ready.pop_front() {
            return Ok(Some(self.buffer(s)));
        }
        let eof = self.eof;
        let Some(p) = self.process.as_mut() else { return Ok(None) };
        if let Some(s) = p.try_recv() {
            return Ok(Some(self.buffer(s)));
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
                Ok(s) => return Ok(Some(self.buffer(s))),
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
            let samples = buf[..whole].chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
            if tx.send(samples).is_err() {
                return;
            }
        }
    }
}
