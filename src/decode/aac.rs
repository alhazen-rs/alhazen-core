//! AAC decoding via rusty_aac (pure Rust, Apache-2.0): AAC-LC, Main, LTP, HE-AAC v1 (SBR) and
//! v2 (PS). Output is interleaved f32 in WAVE channel order; the encoder's start-up padding
//! (`codec_delay`) is trimmed.

use std::collections::VecDeque;

use super::DelayTrim;
use super::audio::{AudioBuffer, AudioDecoder};
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

pub struct AacAudioDecoder {
    asc: Vec<u8>,
    inner: rusty_aac::AacDecoder,
    trim: DelayTrim,
    out: VecDeque<AudioBuffer>,
}

impl AacAudioDecoder {
    /// Set up from the AudioSpecificConfig (Matroska CodecPrivate / MP4 esds).
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let asc = stream.extradata.clone().ok_or_else(|| Error::Decode("aac: missing AudioSpecificConfig".into()))?;
        let inner = decoder(&asc)?;
        Ok(Self { asc, inner, trim: DelayTrim::new(stream.codec_delay), out: VecDeque::new() })
    }
}

fn decoder(asc: &[u8]) -> Result<rusty_aac::AacDecoder> {
    // USAC (xHE-AAC) and other unsupported configurations are refused here, so the stream can
    // go to another backend.
    rusty_aac::AacDecoder::with_config_bytes(asc).map_err(|e| Error::Decode(format!("aac config: {e:?}")))
}

impl AudioDecoder for AacAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.trim.on_packet(packet.pts);
        let frame = self.inner.decode(&packet.data, None).map_err(|e| Error::Decode(format!("aac: {e:?}")))?;
        if let Some(b) = self.trim.apply(frame.samples, frame.channels, frame.sample_rate, packet.pts) {
            self.out.push_back(b);
        }
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) {
        self.out.clear();
        // rusty_aac has no reset; a fresh decoder from the same config is equivalent.
        if let Ok(d) = decoder(&self.asc) {
            self.inner = d;
        }
        self.trim.reset();
    }
}
