//! `AudioDecoder` over a Media Foundation decoder transform, float PCM out.

use std::collections::VecDeque;

use windows::Win32::Media::MediaFoundation::*;

use super::super::select::MfCodec;
use super::super::setup::{audio_bits, audio_user_data, frame_duration};
use super::mft::{self, Output, err};
use super::{codecs, runtime};
use crate::decode::{AudioBuffer, AudioDecoder};
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

struct State {
    mft: IMFTransform,
    name: String,
    rate: u32,
    channels: u16,
    /// Output sample format: 0 = f32, else integer PCM bits per sample (16, 24, 32).
    int_bits: u32,
}

pub struct MfAudioDecoder {
    codec: MfCodec,
    stream: StreamInfo,
    state: Option<State>,
    ready: VecDeque<AudioBuffer>,
}

// SAFETY: as `MfVideoDecoder`: created lazily on the decode thread, used only through
// `&mut self` from there.
unsafe impl Send for MfAudioDecoder {}

impl MfAudioDecoder {
    pub fn new(codec: MfCodec, stream: &StreamInfo) -> Result<Self> {
        Ok(Self { codec, stream: stream.clone(), state: None, ready: VecDeque::new() })
    }

    pub fn description(&self) -> Option<String> {
        self.state.as_ref().map(|s| s.name.clone())
    }

    fn state(&mut self) -> Result<&mut State> {
        if self.state.is_none() {
            runtime::com_init();
            runtime::ensure_started()?;
            let (category, major, subtype) = codecs::ids(self.codec);
            let activate = mft::find_decoder(category, major, subtype, codecs::outputs(self.codec))
                .ok_or_else(|| Error::Decode(format!("no Media Foundation decoder for {:?}", self.codec)))?;
            let name = mft::friendly_name(&activate);
            let mft = mft::activate(&activate)?;
            let s = &self.stream;
            // SAFETY: COM calls on live objects.
            let (rate, channels, int_bits) = unsafe {
                let t = MFCreateMediaType().map_err(err("media type"))?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(err("major type"))?;
                t.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(err("subtype"))?;
                if s.sample_rate > 0 {
                    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, s.sample_rate).map_err(err("rate"))?;
                }
                if s.channels > 0 {
                    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, s.channels as u32).map_err(err("channels"))?;
                }
                if self.codec == MfCodec::Aac {
                    t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(err("aac payload"))?;
                }
                if let Some(bits) = audio_bits(&s.codec, s.extradata.as_deref()) {
                    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits).map_err(err("bits per sample"))?;
                }
                if let Some(data) = audio_user_data(&s.codec, s.extradata.as_deref()) {
                    t.SetBlob(&MF_MT_USER_DATA, &data).map_err(err("codec data"))?;
                }
                mft.SetInputType(0, &t, 0).map_err(err("input type"))?;
                // Float if offered (most decoders), else integer PCM (the Dolby decoders).
                let (mut float, mut pcm, mut offered) = (None, None, Vec::new());
                for i in 0.. {
                    let o = match mft.GetOutputAvailableType(0, i) {
                        Ok(o) => o,
                        Err(e) => {
                            if i == 0 {
                                offered.push(format!("none ({e})"));
                            }
                            break;
                        }
                    };
                    let sub = o.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
                    offered.push(format!("{sub:?}/{}bit", o.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(0)));
                    if sub == MFAudioFormat_Float && float.is_none() {
                        float = Some(o);
                    } else if sub == MFAudioFormat_PCM
                        && pcm.is_none()
                        && matches!(o.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(0), 16 | 24 | 32)
                    {
                        pcm = Some(o);
                    }
                }
                let (out, int_bits) = match (float, pcm) {
                    (Some(o), _) => (o, 0),
                    (None, Some(o)) => {
                        let bits = o.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).map_err(err("bits"))?;
                        (o, bits)
                    }
                    (None, None) => {
                        return Err(Error::Decode(format!(
                            "{name} offers no float or PCM output (offers: {})",
                            offered.join(", ")
                        )));
                    }
                };
                mft.SetOutputType(0, &out, 0).map_err(err("output type"))?;
                (
                    out.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).map_err(err("output rate"))?,
                    out.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).map_err(err("output channels"))? as u16,
                    int_bits,
                )
            };
            mft::begin_streaming(&mft)?;
            log::info!("Media Foundation {:?}: {name}", self.codec);
            self.state = Some(State { mft, name, rate, channels, int_bits });
        }
        Ok(self.state.as_mut().unwrap())
    }

    fn pull_one(&mut self) -> Result<bool> {
        let Some(s) = self.state.as_ref() else { return Ok(false) };
        match mft::process_output(&s.mft)? {
            Output::Sample(sample) => {
                // SAFETY: COM call on a live sample.
                let pts = mft::from_mf_time(unsafe { sample.GetSampleTime() }.unwrap_or(0));
                let bytes = mft::sample_bytes(&sample)?;
                let samples = pcm_to_f32(&bytes, s.int_bits);
                self.ready.push_back(AudioBuffer { rate: s.rate, channels: s.channels, samples, pts });
                Ok(true)
            }
            Output::NeedMoreInput | Output::StreamChange => Ok(false),
        }
    }
}

impl AudioDecoder for MfAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let mft = self.state()?.mft.clone();
        let duration = frame_duration(&self.stream.codec, self.stream.extradata.as_deref(), self.stream.sample_rate);
        let sample = mft::sample(&packet.data, packet.pts, duration)?;
        // SAFETY: COM call on a live transform.
        match unsafe { mft.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                while self.pull_one()? {}
                // SAFETY: as above.
                unsafe { mft.ProcessInput(0, &sample, 0) }.map_err(err("decode input"))
            }
            Err(e) => Err(err("decode input")(e)),
        }
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        if self.ready.is_empty() {
            self.pull_one()?;
        }
        Ok(self.ready.pop_front())
    }

    fn send_eof(&mut self) {
        if let Some(s) = &self.state {
            mft::drain(&s.mft);
        }
    }

    fn flush(&mut self) {
        self.ready.clear();
        if let Some(s) = &self.state {
            mft::flush(&s.mft);
            let _ = mft::begin_streaming(&s.mft);
        }
    }
}

/// Little-endian samples to f32: `int_bits` 0 means already f32, else signed integer PCM.
fn pcm_to_f32(bytes: &[u8], int_bits: u32) -> Vec<f32> {
    match int_bits {
        0 => bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect(),
        16 => bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32_768.0).collect(),
        24 => bytes.as_chunks::<3>().0.iter().map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0).collect(),
        _ => bytes.as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b) as f32 / 2_147_483_648.0).collect(),
    }
}
