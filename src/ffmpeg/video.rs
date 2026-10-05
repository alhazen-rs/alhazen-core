//! `VideoDecoder` that runs ffmpeg as a pure decoder: Matroska in, and Matroska with raw 8-bit
//! 4:2:0 frames out. Output blocks carry the input timestamps (`-copyts`), so every frame is
//! matched to the packet it came from even when ffmpeg skips frames (open-GOP leading pictures
//! after a seek, VP8 alt-refs, corrupt frames).

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::process::ChildStdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender};

use super::locate::FfmpegInfo;
use super::mkv::{BlockWriter, header};
use super::process::FfmpegProcess;
use crate::decode::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, chroma_size};
use crate::demux::{Demuxer, MatroskaDemuxer, Packet, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

/// How long draining at end of stream waits for ffmpeg's next frame before calling it stalled.
const STALL: Duration = Duration::from_secs(10);

/// A picture from ffmpeg's output, or the reason the stream stopped.
pub(crate) enum Raw {
    /// `ms`: the block timestamp, i.e. the input packet's pts in whole milliseconds. Colour
    /// information as ffmpeg tagged its output.
    Frame { width: u32, height: u32, data: Vec<u8>, ms: u64, matrix: Option<u8>, full_range: Option<bool> },
    Bad(String),
}

pub struct FfmpegVideoDecoder {
    info: Arc<FfmpegInfo>,
    stream: StreamInfo,
    hwaccel: bool,
    process: Option<FfmpegProcess<Raw>>,
    blocks: BlockWriter,
    /// Pts of packets sent and not yet matched to an output frame, by whole millisecond (the
    /// precision of the Matroska round trip).
    pending: BTreeMap<u64, Duration>,
    ready: VecDeque<Raw>,
    eof: bool,
}

impl FfmpegVideoDecoder {
    pub fn new(info: Arc<FfmpegInfo>, stream: &StreamInfo, hwaccel: bool) -> Result<Self> {
        header(stream).ok_or(Error::Unsupported("codec cannot be passed to ffmpeg"))?;
        Ok(Self {
            info,
            stream: stream.clone(),
            hwaccel,
            process: None,
            blocks: BlockWriter::default(),
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
            eof: false,
        })
    }

    fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = ["-hide_banner", "-nostats", "-loglevel", "error"].map(String::from).to_vec();
        if self.hwaccel {
            a.extend(["-hwaccel", "auto"].map(String::from));
        }
        // The Matroska header already describes the stream: don't buffer seconds of input to probe.
        a.extend(["-probesize", "32768", "-analyzeduration", "1"].map(String::from));
        a.extend(["-f", "matroska", "-i", "pipe:0", "-map", "0:v:0", "-an", "-sn"].map(String::from));
        a.extend(self.info.passthrough_args().map(String::from));
        a.extend(["-copyts", "-c:v", "rawvideo", "-pix_fmt", "yuv420p", "-f", "matroska", "pipe:1"].map(String::from));
        a
    }

    fn process(&mut self) -> Result<&mut FfmpegProcess<Raw>> {
        if self.process.is_none() {
            let head = header(&self.stream).expect("checked in new");
            self.process = Some(FfmpegProcess::spawn(&self.info.path, &self.args(), head, 4, read_raw)?);
            self.blocks = BlockWriter::default();
        }
        Ok(self.process.as_mut().unwrap())
    }

    fn make_frame(&mut self, out: Raw) -> Result<DecodedFrame> {
        let (width, height, data, ms, matrix, full_range) = match out {
            Raw::Frame { width, height, data, ms, matrix, full_range } => (width, height, data, ms, matrix, full_range),
            Raw::Bad(msg) => return Err(Error::Decode(format!("ffmpeg output: {msg}"))),
        };
        let pts = match_pts(&mut self.pending, ms);
        let (cw, ch) = chroma_size(PixelLayout::I420, width, height);
        let (ys, cs) = ((width * height) as usize, (cw * ch) as usize);
        let mut data = data;
        let v = data.split_off(ys + cs);
        let u = data.split_off(ys);
        Ok(DecodedFrame::Yuv(YuvFrame {
            width,
            height,
            layout: PixelLayout::I420,
            planes: [data, u, v],
            strides: [width as usize, cw as usize, cw as usize],
            matrix: matrix.and_then(ColorMatrix::from_h273).unwrap_or_else(|| ColorMatrix::guess_for_height(height)),
            full_range: full_range.unwrap_or(false),
            pts,
        }))
    }
}

impl VideoDecoder for FfmpegVideoDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.eof = false;
        // Start ffmpeg (and a fresh block writer) before encoding the packet: a new run's first
        // block must open a Cluster.
        self.process()?;
        let bytes = self.blocks.block(packet.pts, packet.keyframe, &packet.data);
        let mut ready = std::mem::take(&mut self.ready);
        let result = self.process.as_mut().unwrap().write(bytes, &mut ready);
        self.ready = ready;
        self.pending.insert(packet.pts.as_millis() as u64, packet.pts);
        result
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        if let Some(out) = self.ready.pop_front() {
            return self.make_frame(out).map(Some);
        }
        let eof = self.eof;
        let Some(p) = self.process.as_mut() else { return Ok(None) };
        if let Some(out) = p.try_recv() {
            return self.make_frame(out).map(Some);
        }
        if !eof {
            if p.is_drained() {
                // stdout ended while we were still feeding it: ffmpeg died.
                let err = p.failure("ffmpeg exited during decoding");
                self.process = None;
                return Err(err);
            }
            return Ok(None);
        }
        // End of stream: wait for ffmpeg to flush its delayed frames, then exit.
        let started = Instant::now();
        loop {
            match p.recv_timeout(Duration::from_millis(50)) {
                Ok(out) => return self.make_frame(out).map(Some),
                Err(RecvTimeoutError::Disconnected) => {
                    let failed = p.exit_failed();
                    let err = failed.then(|| p.failure("ffmpeg failed"));
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
        self.pending.clear();
        self.ready.clear();
        self.eof = false;
    }
}

/// How far ffmpeg's output timestamp may be from the input one: it rescales through the
/// encoder's time base (66 ms in can come back as 67 ms).
const MATCH_TOLERANCE_MS: u64 = 2;

/// The pts of the packet whose frame ffmpeg output at `ms` (the nearest pending one within the
/// tolerance). Earlier packets still pending made no frame (ffmpeg skipped them) and are forgotten.
fn match_pts(pending: &mut BTreeMap<u64, Duration>, ms: u64) -> Duration {
    let near = pending
        .range(ms.saturating_sub(MATCH_TOLERANCE_MS)..=ms + MATCH_TOLERANCE_MS)
        .min_by_key(|(k, _)| k.abs_diff(ms))
        .map(|(k, _)| *k);
    let cut = near.unwrap_or(ms.saturating_sub(MATCH_TOLERANCE_MS));
    let later = pending.split_off(&cut);
    *pending = later;
    match near {
        Some(k) => pending.remove(&k).unwrap(),
        None => Duration::from_millis(ms),
    }
}

/// ffmpeg's stdout as a forward-only source for our Matroska demuxer.
struct Pipe(BufReader<ChildStdout>);

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for Pipe {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "pipe"))
    }
}

impl MediaSource for Pipe {
    fn byte_len(&self) -> Option<u64> {
        None
    }
    fn is_seekable(&self) -> bool {
        false
    }
    fn is_live(&self) -> bool {
        false
    }
    fn description(&self) -> String {
        "ffmpeg output".into()
    }
}

/// Demuxes ffmpeg's Matroska output into frames.
pub(crate) fn read_raw(stdout: ChildStdout, tx: Sender<Raw>) {
    let Ok(mut demuxer) = MatroskaDemuxer::open(Box::new(Pipe(BufReader::with_capacity(1 << 20, stdout)))) else {
        return; // no output at all: the caller reports ffmpeg's stderr
    };
    let Some(track) = demuxer.streams().iter().find(|s| s.kind == StreamKind::Video).cloned() else {
        let _ = tx.send(Raw::Bad("no video track in ffmpeg's output".into()));
        return;
    };
    let (width, height) = (track.width, track.height);
    let (cw, ch) = chroma_size(PixelLayout::I420, width, height);
    let size = (width * height + 2 * cw * ch) as usize;
    while let Ok(Some(p)) = demuxer.next_packet() {
        if p.stream != track.id {
            continue;
        }
        if size == 0 || p.data.len() != size {
            let _ = tx.send(Raw::Bad(format!("{} byte frame for {width}x{height}", p.data.len())));
            return;
        }
        let ms = p.pts.as_millis() as u64;
        let (matrix, full_range) = (track.color_matrix, track.full_range);
        if tx.send(Raw::Frame { width, height, data: p.data, ms, matrix, full_range }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(ms: &[u64]) -> BTreeMap<u64, Duration> {
        ms.iter().map(|&m| (m, Duration::from_micros(m * 1000 + 333))).collect()
    }

    #[test]
    fn frames_take_their_own_packets_pts() {
        let mut p = pending(&[0, 33, 67, 100]);
        assert_eq!(match_pts(&mut p, 33), Duration::from_micros(33_333), "0 was skipped by ffmpeg");
        assert_eq!(p.keys().copied().collect::<Vec<_>>(), [67, 100]);
        assert_eq!(match_pts(&mut p, 100), Duration::from_micros(100_333));
        assert!(p.is_empty());
    }

    #[test]
    fn timestamps_rounded_by_ffmpeg_still_match() {
        let mut p = pending(&[0, 33, 66, 100]);
        assert_eq!(match_pts(&mut p, 67), Duration::from_micros(66_333), "66 ms came back as 67");
        assert_eq!(p.keys().copied().collect::<Vec<_>>(), [100]);
    }

    #[test]
    fn an_unknown_timestamp_is_used_as_is() {
        let mut p = pending(&[200]);
        assert_eq!(match_pts(&mut p, 150), Duration::from_millis(150));
        assert_eq!(p.len(), 1, "later packets stay pending");
    }
}
