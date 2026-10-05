//! AAC-LC decoding via Symphonia (MPL-2.0; only built with the `native-aac` feature).

use std::collections::VecDeque;

use symphonia_codec_aac::AacDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder as _, AudioDecoderOptions};
use symphonia_core::packet::Packet as SymPacket;
use symphonia_core::units::{Duration as SymDuration, Timestamp};

use super::audio::{AudioBuffer, AudioDecoder};
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

pub struct AacAudioDecoder {
    inner: AacDecoder,
    out: VecDeque<AudioBuffer>,
}

impl AacAudioDecoder {
    /// Set up from the AudioSpecificConfig (Matroska CodecPrivate / MP4 esds).
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let asc = stream.extradata.clone().ok_or_else(|| Error::Decode("aac: missing AudioSpecificConfig".into()))?;
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC).with_extra_data(asc.into_boxed_slice());
        if stream.sample_rate > 0 {
            params.with_sample_rate(stream.sample_rate);
        }
        let inner = AacDecoder::try_new(&params, &AudioDecoderOptions::default())
            .map_err(|e| Error::Decode(format!("aac init: {e}")))?;
        Ok(Self { inner, out: VecDeque::new() })
    }
}

impl AudioDecoder for AacAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let p = SymPacket::new(0, Timestamp::new(0), SymDuration::new(0), packet.data.clone());
        let decoded = self.inner.decode(&p).map_err(|e| Error::Decode(format!("aac: {e}")))?;
        let rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count() as u16;
        let mut samples = Vec::new();
        decoded.copy_to_vec_interleaved::<f32>(&mut samples);
        if !samples.is_empty() {
            self.out.push_back(AudioBuffer { rate, channels, samples, pts: packet.pts });
        }
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) {
        self.out.clear();
        self.inner.reset();
    }
}
