//! `VideoDecoder` that runs ffmpeg as a pure decoder: Matroska in, YUV4MPEG2 (4:2:0, 8-bit) out.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::ChildStdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender};

use super::locate::FfmpegInfo;
use super::mkv::{BlockWriter, header};
use super::process::{FfmpegProcess, read_full};
use crate::decode::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, chroma_size};
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

/// How long draining at end of stream waits for ffmpeg's next frame before calling it stalled.
const STALL: Duration = Duration::from_secs(10);

/// A picture from ffmpeg's y4m output, or the reason the stream stopped.
pub(crate) enum Y4m {
    Frame { width: u32, height: u32, data: Vec<u8> },
    Bad(String),
}

pub struct FfmpegVideoDecoder {
    info: Arc<FfmpegInfo>,
    stream: StreamInfo,
    hwaccel: bool,
    process: Option<FfmpegProcess<Y4m>>,
    blocks: BlockWriter,
    /// Pts of packets sent and not yet matched to an output frame, smallest first: frames
    /// come out in presentation order.
    pending: BinaryHeap<Reverse<Duration>>,
    ready: VecDeque<Y4m>,
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
            pending: BinaryHeap::new(),
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
        a.extend(["-pix_fmt", "yuv420p", "-f", "yuv4mpegpipe", "pipe:1"].map(String::from));
        a
    }

    fn process(&mut self) -> Result<&mut FfmpegProcess<Y4m>> {
        if self.process.is_none() {
            let head = header(&self.stream).expect("checked in new");
            self.process = Some(FfmpegProcess::spawn(&self.info.path, &self.args(), head, 4, read_y4m)?);
            self.blocks = BlockWriter::default();
        }
        Ok(self.process.as_mut().unwrap())
    }

    fn make_frame(&mut self, out: Y4m) -> Result<DecodedFrame> {
        let (width, height, data) = match out {
            Y4m::Frame { width, height, data } => (width, height, data),
            Y4m::Bad(msg) => return Err(Error::Decode(format!("ffmpeg output: {msg}"))),
        };
        let pts = self.pending.pop().map(|Reverse(p)| p).unwrap_or_default();
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
            matrix: ColorMatrix::guess_for_height(height),
            full_range: false,
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
        self.pending.push(Reverse(packet.pts));
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

/// Parses a YUV4MPEG2 stream: one header line, then `FRAME` lines each followed by a picture.
pub(crate) fn read_y4m(stdout: ChildStdout, tx: Sender<Y4m>) {
    parse_y4m(BufReader::with_capacity(1 << 20, stdout), &tx);
}

pub(crate) fn parse_y4m(mut r: impl BufRead, tx: &Sender<Y4m>) {
    let mut line = Vec::new();
    if r.by_ref().take(1024).read_until(b'\n', &mut line).unwrap_or(0) == 0 {
        return; // no output at all: the caller reports ffmpeg's stderr
    }
    let header = String::from_utf8_lossy(&line).into_owned();
    let Some(params) = header.trim_end().strip_prefix("YUV4MPEG2") else {
        let _ = tx.send(Y4m::Bad(format!("not a y4m stream: {:?}", header.trim_end())));
        return;
    };
    let (mut width, mut height, mut chroma) = (0u32, 0u32, "420jpeg".to_owned());
    for p in params.split_whitespace() {
        match p.split_at(1) {
            ("W", v) => width = v.parse().unwrap_or(0),
            ("H", v) => height = v.parse().unwrap_or(0),
            ("C", v) => chroma = v.to_owned(),
            _ => {}
        }
    }
    // 8-bit 4:2:0 is `420`, `420jpeg`, `420mpeg2`, `420paldv`; high bit depth is `420p10` etc.
    if width == 0 || height == 0 || !chroma.starts_with("420") || chroma.starts_with("420p1") {
        let _ = tx.send(Y4m::Bad(format!("unexpected y4m header {:?}", header.trim_end())));
        return;
    }
    let (cw, ch) = chroma_size(PixelLayout::I420, width, height);
    let size = (width * height + 2 * cw * ch) as usize;
    loop {
        line.clear();
        if r.by_ref().take(1024).read_until(b'\n', &mut line).unwrap_or(0) == 0 {
            return;
        }
        if !line.starts_with(b"FRAME") {
            let _ = tx.send(Y4m::Bad("missing FRAME marker".into()));
            return;
        }
        let mut data = vec![0u8; size];
        if !read_full(&mut r, &mut data) {
            return;
        }
        if tx.send(Y4m::Frame { width, height, data }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Y4m> {
        let (tx, rx) = crossbeam_channel::unbounded();
        parse_y4m(bytes, &tx);
        drop(tx);
        rx.iter().collect()
    }

    #[test]
    fn parses_frames_of_odd_size() {
        // 3x3: Y 9 bytes, U/V 2x2 = 4 bytes each.
        let mut s = b"YUV4MPEG2 W3 H3 F30:1 Ip A1:1 C420jpeg XYSCSS=420JPEG\n".to_vec();
        for i in 0..2u8 {
            s.extend_from_slice(b"FRAME\n");
            s.extend(std::iter::repeat_n(i, 17));
        }
        let out = parse(&s);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[1], Y4m::Frame { width: 3, height: 3, data } if data.len() == 17 && data[0] == 1));
    }

    #[test]
    fn truncated_frame_is_dropped_and_wrong_format_reported() {
        let mut s = b"YUV4MPEG2 W2 H2 C420mpeg2\nFRAME\n".to_vec();
        s.extend([0u8; 3]);
        assert!(parse(&s).is_empty());
        assert!(matches!(&parse(b"YUV4MPEG2 W2 H2 C444\n")[0], Y4m::Bad(_)));
        assert!(matches!(&parse(b"YUV4MPEG2 W2 H2 C420p10\n")[0], Y4m::Bad(_)));
        assert!(matches!(&parse(b"garbage\n")[0], Y4m::Bad(_)));
        assert!(parse(b"").is_empty());
    }
}
