//! `AudioDecoder` over a Media Foundation decoder transform, float PCM out.

use std::collections::VecDeque;

use windows::Win32::Media::MediaFoundation::*;

use super::super::select::MfCodec;
use super::super::setup::audio_user_data;
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
            let activate = mft::find_decoder(category, major, subtype)
                .ok_or_else(|| Error::Decode(format!("no Media Foundation decoder for {:?}", self.codec)))?;
            let name = mft::friendly_name(&activate);
            let mft = mft::activate(&activate)?;
            let s = &self.stream;
            // SAFETY: COM calls on live objects.
            let (rate, channels) = unsafe {
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
                if let Some(data) = audio_user_data(&s.codec, s.extradata.as_deref()) {
                    t.SetBlob(&MF_MT_USER_DATA, &data).map_err(err("codec data"))?;
                }
                mft.SetInputType(0, &t, 0).map_err(err("input type"))?;
                let mut out = None;
                for i in 0.. {
                    let Ok(o) = mft.GetOutputAvailableType(0, i) else { break };
                    if o.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() == MFAudioFormat_Float {
                        out = Some(o);
                        break;
                    }
                }
                let out = out.ok_or_else(|| Error::Decode("Media Foundation offers no float output".into()))?;
                mft.SetOutputType(0, &out, 0).map_err(err("output type"))?;
                (
                    out.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).map_err(err("output rate"))?,
                    out.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).map_err(err("output channels"))? as u16,
                )
            };
            mft::begin_streaming(&mft)?;
            log::info!("Media Foundation {:?}: {name}", self.codec);
            self.state = Some(State { mft, name, rate, channels });
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
                let samples = bytes.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
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
        let sample = mft::sample(&packet.data, packet.pts)?;
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
