//! FLAC decoding via `claxon` (pure Rust). Matroska and MP4 store one FLAC frame per packet; the
//! STREAMINFO header (CodecPrivate / `dfLa`, as `fLaC` + metadata blocks) gives the bit depth.

use std::collections::VecDeque;
use std::io::Cursor;

use claxon::frame::FrameReader;

use super::audio::{AudioBuffer, AudioDecoder};
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

pub struct FlacAudioDecoder {
    rate: u32,
    /// Bits per sample (4–32): decoded integers are scaled by 2^(bits − 1).
    bits: u32,
    /// Reused sample buffer (claxon decodes into it and hands it back).
    buffer: Vec<i32>,
    out: VecDeque<AudioBuffer>,
}

impl FlacAudioDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let header = stream.extradata.as_deref().ok_or_else(|| Error::Decode("flac: missing STREAMINFO".into()))?;
        let (rate, bits) = streaminfo(header).ok_or_else(|| Error::Decode("flac: malformed STREAMINFO".into()))?;
        Ok(Self { rate, bits, buffer: Vec::new(), out: VecDeque::new() })
    }
}

/// (sample rate, bits per sample) from `fLaC` + the STREAMINFO metadata block.
fn streaminfo(header: &[u8]) -> Option<(u32, u32)> {
    let blocks = header.strip_prefix(b"fLaC")?;
    if blocks.first()? & 0x7F != 0 {
        return None; // STREAMINFO must come first
    }
    let info = blocks.get(4..4 + 34)?;
    // Bytes 10..14: sample rate (20 bits), channels − 1 (3), bits per sample − 1 (5), ...
    let rate = (info[10] as u32) << 12 | (info[11] as u32) << 4 | (info[12] as u32) >> 4;
    let bits = ((info[12] as u32 & 1) << 4 | (info[13] as u32) >> 4) + 1;
    Some((rate, bits))
}

impl AudioDecoder for FlacAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut reader = FrameReader::new(Cursor::new(&packet.data[..]));
        let buffer = std::mem::take(&mut self.buffer);
        let block = match reader.read_next_or_eof(buffer) {
            Ok(Some(block)) => block,
            Ok(None) => return Ok(()),
            Err(e) => return Err(Error::Decode(format!("flac: {e}"))),
        };
        let (channels, frames) = (block.channels(), block.duration());
        let scale = 1.0 / (1u64 << (self.bits - 1)) as f32;
        let mut samples = Vec::with_capacity((channels * frames) as usize);
        for i in 0..frames {
            // FLAC's channel order for 1–8 channels is already WAVE order.
            for ch in 0..channels {
                samples.push(block.sample(ch, i) as f32 * scale);
            }
        }
        self.buffer = block.into_buffer();
        self.out.push_back(AudioBuffer { rate: self.rate, channels: channels as u16, samples, pts: packet.pts });
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) {
        self.out.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaminfo_gives_rate_and_bits() {
        // 48000 Hz = 0x0BB80, stereo (1), 24 bits (23): bytes 10..14 = 0B B8 03 70.
        let mut h = b"fLaC".to_vec();
        h.extend_from_slice(&[0x80, 0, 0, 34]);
        let mut info = [0u8; 34];
        info[10..14].copy_from_slice(&[0x0B, 0xB8, 0x03, 0x70]);
        h.extend_from_slice(&info);
        assert_eq!(streaminfo(&h), Some((48_000, 24)));
        assert_eq!(streaminfo(b"fLaC\x84\0\0\x22"), None);
        assert_eq!(streaminfo(b"junk"), None);
    }
}
