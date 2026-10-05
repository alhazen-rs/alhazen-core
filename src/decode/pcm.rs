//! Uncompressed PCM: bytes to interleaved f32.

use std::collections::VecDeque;

use super::{AudioBuffer, AudioDecoder};
use crate::demux::{Codec, Packet, PcmFormat, StreamInfo};
use crate::{Error, Result};

pub struct PcmAudioDecoder {
    format: PcmFormat,
    rate: u32,
    channels: u16,
    ready: VecDeque<AudioBuffer>,
}

impl PcmAudioDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let Codec::Pcm(format) = stream.codec else {
            return Err(Error::Unsupported("not PCM"));
        };
        let ok = if format.float { matches!(format.bits, 32 | 64) } else { matches!(format.bits, 8 | 16 | 24 | 32) };
        if !ok || stream.sample_rate == 0 || stream.channels == 0 {
            return Err(Error::Decode(format!("unsupported {} ({} Hz, {} channels)", stream.codec, stream.sample_rate, stream.channels)));
        }
        Ok(Self { format, rate: stream.sample_rate, channels: stream.channels, ready: VecDeque::new() })
    }
}

impl AudioDecoder for PcmAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let width = self.format.bits as usize / 8;
        let frame = width * self.channels as usize;
        // A trailing partial frame (truncated file) is dropped.
        let whole = packet.data.len() - packet.data.len() % frame;
        if whole == 0 {
            return Ok(());
        }
        let samples = packet.data[..whole].chunks_exact(width).map(|b| to_f32(b, self.format)).collect();
        self.ready.push_back(AudioBuffer { rate: self.rate, channels: self.channels, samples, pts: packet.pts });
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.ready.pop_front())
    }

    fn flush(&mut self) {
        self.ready.clear();
    }
}

/// One sample (`b.len()` = bytes per sample) as f32 in [-1, 1].
fn to_f32(b: &[u8], f: PcmFormat) -> f32 {
    let mut bytes = [0u8; 8];
    let n = b.len();
    // Big-endian into the low `n` bytes of a little-endian buffer.
    for i in 0..n {
        bytes[i] = if f.big_endian { b[n - 1 - i] } else { b[i] };
    }
    if f.float {
        return match n {
            4 => f32::from_le_bytes(bytes[..4].try_into().unwrap()),
            _ => f64::from_le_bytes(bytes) as f32,
        };
    }
    let raw = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let bits = n as u32 * 8;
    let value = if f.signed {
        // Sign-extend from `bits`.
        ((raw << (32 - bits)) as i32 >> (32 - bits)) as f64
    } else {
        raw as f64 - (1u64 << (bits - 1)) as f64
    };
    (value / (1u64 << (bits - 1)) as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_every_layout() {
        let s24 = PcmFormat::int(24, false, true);
        assert_eq!(to_f32(&[0x00, 0x00, 0x80], s24), -1.0);
        assert_eq!(to_f32(&[0x00, 0x00, 0x40], s24), 0.5);
        assert_eq!(to_f32(&[0x40, 0x00, 0x00], PcmFormat::int(24, true, true)), 0.5);
        assert_eq!(to_f32(&[0xFF, 0x7F], PcmFormat::int(16, false, true)), 32767.0 / 32768.0);
        assert_eq!(to_f32(&[0x80, 0x00], PcmFormat::int(16, true, true)), -1.0);
        assert_eq!(to_f32(&[0x80], PcmFormat::int(8, false, false)), 0.0, "8-bit offset binary");
        assert_eq!(to_f32(&[0x00], PcmFormat::int(8, false, false)), -1.0);
        assert_eq!(to_f32(&0.25f32.to_le_bytes(), PcmFormat::float(32, false)), 0.25);
        assert_eq!(to_f32(&(-0.5f64).to_be_bytes(), PcmFormat::float(64, true)), -0.5);
        assert_eq!(to_f32(&i32::MIN.to_le_bytes(), PcmFormat::int(32, false, true)), -1.0);
    }

    #[test]
    fn partial_trailing_frame_is_dropped() {
        let mut s = StreamInfo::new(1, crate::demux::StreamKind::Audio, Codec::Pcm(PcmFormat::int(16, false, true)));
        (s.sample_rate, s.channels) = (48_000, 2);
        let mut d = PcmAudioDecoder::new(&s).unwrap();
        let p = Packet { stream: 1, pts: std::time::Duration::ZERO, keyframe: true, data: vec![0; 4 * 3 + 2], generation: 0 };
        d.send_packet(&p).unwrap();
        assert_eq!(d.receive_samples().unwrap().unwrap().samples.len(), 6);
    }
}
