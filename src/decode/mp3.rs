//! MP3 decoding via `rusty_mp3` (pure Rust, Apache-2.0). One packet is one MPEG audio frame (from
//! the MP3 reader, MP4 or Matroska). Start-up padding (LAME delay or container delay) and end
//! padding are trimmed by `DelayTrim`.

use std::collections::VecDeque;
use std::time::Duration;

use super::DelayTrim;
use super::audio::{AudioBuffer, AudioDecoder};
use crate::Result;
use crate::demux::{Packet, StreamInfo};

pub struct Mp3AudioDecoder {
    inner: rusty_mp3::Mp3Decoder,
    trim: DelayTrim,
    out: VecDeque<AudioBuffer>,
}

impl Mp3AudioDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        Ok(Self {
            inner: rusty_mp3::Mp3Decoder::new(),
            trim: DelayTrim::new(stream.codec_delay).with_end(stream.end_trim),
            out: VecDeque::new(),
        })
    }
}

impl AudioDecoder for Mp3AudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.trim.on_packet(packet.pts);
        // `push` decodes every frame the bytes complete; a corrupt frame is skipped by the
        // decoder's own frame sync.
        self.inner.push(&packet.data);
        let mut pts = packet.pts;
        while let Ok(frame) = self.inner.next_frame() {
            let frames = frame.samples.len() / frame.channels.max(1) as usize;
            let next = pts + Duration::from_secs_f64(frames as f64 / frame.sample_rate.max(1) as f64);
            if let Some(b) = self.trim.apply(frame.samples, frame.channels, frame.sample_rate, pts) {
                self.out.push_back(b);
            }
            pts = next;
        }
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) {
        // rusty_mp3 has no reset: a new decoder forgets the bit reservoir and overlap.
        self.inner = rusty_mp3::Mp3Decoder::new();
        self.out.clear();
        self.trim.reset();
    }
}
