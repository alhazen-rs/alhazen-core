//! Vorbis decoding via `lewton` (pure Rust), fed raw packets.

use std::collections::VecDeque;

use lewton::audio::{PreviousWindowRight, read_audio_packet_generic};
use lewton::header::{IdentHeader, SetupHeader, read_header_ident, read_header_setup};
use lewton::samples::InterleavedSamples;

use super::audio::{AudioBuffer, AudioDecoder};
use super::channels::to_wave_order;
use crate::demux::{Packet, StreamInfo, split_xiph_lacing};
use crate::{Error, Result};

pub struct VorbisAudioDecoder {
    ident: IdentHeader,
    setup: SetupHeader,
    pwr: PreviousWindowRight,
    out: VecDeque<AudioBuffer>,
}

impl VorbisAudioDecoder {
    /// Set up from Matroska CodecPrivate: the three Vorbis headers, Xiph-laced.
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let private = stream.extradata.as_deref().ok_or_else(|| Error::Decode("vorbis: missing headers".into()))?;
        let headers = split_xiph_lacing(private).filter(|h| h.len() == 3);
        let headers = headers.ok_or_else(|| Error::Decode("vorbis: malformed CodecPrivate".into()))?;
        let err = |e| Error::Decode(format!("vorbis header: {e:?}"));
        let ident = read_header_ident(headers[0]).map_err(err)?;
        let setup = read_header_setup(headers[2], ident.audio_channels, (ident.blocksize_0, ident.blocksize_1))
            .map_err(err)?;
        Ok(Self { ident, setup, pwr: PreviousWindowRight::new(), out: VecDeque::new() })
    }
}

impl AudioDecoder for VorbisAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let decoded: InterleavedSamples<f32> =
            read_audio_packet_generic(&self.ident, &self.setup, &packet.data, &mut self.pwr)
                .map_err(|e| Error::Decode(format!("vorbis: {e:?}")))?;
        // The first packet after (re)start only primes the overlap window and yields nothing.
        if !decoded.samples.is_empty() {
            let channels = self.ident.audio_channels as u16;
            self.out.push_back(AudioBuffer {
                rate: self.ident.audio_sample_rate,
                channels,
                samples: to_wave_order(decoded.samples, channels),
                pts: packet.pts,
            });
        }
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) {
        self.out.clear();
        self.pwr = PreviousWindowRight::new();
    }
}
