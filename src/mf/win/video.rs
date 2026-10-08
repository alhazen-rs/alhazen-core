//! `VideoDecoder` over a Media Foundation decoder transform, GPU-accelerated (DXVA) when the
//! transform accepts our D3D11 device.

use std::collections::VecDeque;

use windows::Win32::Media::MediaFoundation::*;
use windows::core::{GUID, Interface};

use super::super::select::MfCodec;
use super::mft::{self, Output, err};
use super::{codecs, device, runtime};
use crate::decode::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, chroma_size};
use crate::demux::{Packet, StreamInfo};
use crate::nal::{AnnexB, ParamSetFormat};
use crate::{Error, Result};

/// What the transform outputs.
#[derive(Clone, Copy, Debug)]
struct OutFormat {
    /// Buffer (coded) size, e.g. 1920x1088.
    coded: (u32, u32),
    /// Picture size, e.g. 1920x1080.
    visible: (u32, u32),
    p010: bool,
    matrix: Option<ColorMatrix>,
    full_range: bool,
}

struct State {
    mft: IMFTransform,
    gpu: bool,
    name: String,
    out: OutFormat,
}

pub struct MfVideoDecoder {
    codec: MfCodec,
    stream: StreamInfo,
    allow_gpu: bool,
    annexb: Option<AnnexB>,
    state: Option<State>,
    ready: VecDeque<YuvFrame>,
    scratch: Vec<u8>,
}

// SAFETY: the transform is created lazily on the first packet, on the decode thread that owns
// this decoder from then on, inside a multithreaded COM apartment; it is never used from two
// threads at once (all access is through `&mut self`).
unsafe impl Send for MfVideoDecoder {}

impl MfVideoDecoder {
    pub fn new(codec: MfCodec, stream: &StreamInfo, allow_gpu: bool) -> Result<Self> {
        let annexb = match codec {
            MfCodec::H264 | MfCodec::Hevc => {
                let format = if codec == MfCodec::H264 { ParamSetFormat::Avcc } else { ParamSetFormat::Hvcc };
                // Without a configuration record the stream is already Annex B (or broken).
                stream.extradata.as_deref().and_then(|c| AnnexB::from_config(format, c))
            }
            _ => None,
        };
        Ok(Self { codec, stream: stream.clone(), allow_gpu, annexb, state: None, ready: VecDeque::new(), scratch: Vec::new() })
    }

    /// The transform's name and whether it decodes on the GPU (after the first packet).
    pub fn description(&self) -> Option<(String, bool)> {
        self.state.as_ref().map(|s| (s.name.clone(), s.gpu))
    }

    fn state(&mut self) -> Result<&mut State> {
        if self.state.is_none() {
            runtime::com_init();
            runtime::ensure_started()?;
            let (category, major, subtype) = codecs::ids(self.codec);
            let activate = mft::find_decoder(category, major, subtype, codecs::outputs(self.codec))
                .ok_or_else(|| Error::Decode(format!("no Media Foundation decoder for {:?}", self.codec)))?;
            let name = mft::friendly_name(&activate);
            let gpu = if self.allow_gpu { device::gpu() } else { None };
            let attempt = |with_gpu: bool| -> Result<State> {
                let mft = mft::activate(&activate)?;
                let used_gpu = with_gpu && gpu.is_some_and(|g| mft::use_gpu(&mft, &g.manager));
                set_input(&mft, subtype, &self.stream)?;
                let out = negotiate_output(&mft, &self.stream)?;
                mft::begin_streaming(&mft)?;
                Ok(State { mft, gpu: used_gpu, name: name.clone(), out })
            };
            let state = match attempt(gpu.is_some()) {
                Ok(s) => s,
                Err(e) if gpu.is_some() => {
                    log::warn!("{name} refused GPU decoding ({e}); decoding in software");
                    // A fresh activation: the first one is bound to the refused setup.
                    // SAFETY: COM call on a live activation object.
                    let _ = unsafe { activate.ShutdownObject() };
                    attempt(false)?
                }
                Err(e) => return Err(e),
            };
            log::info!("Media Foundation {:?}: {} ({})", self.codec, state.name, if state.gpu { "GPU" } else { "software" });
            self.state = Some(state);
        }
        Ok(self.state.as_mut().unwrap())
    }

    /// Pulls every available frame into `ready`; returns once the transform wants input.
    fn pull(&mut self) -> Result<()> {
        loop {
            let state = self.state.as_mut().unwrap();
            match mft::process_output(&state.mft)? {
                Output::Sample(s) => {
                    let f = read_frame(&s, &state.out)?;
                    self.ready.push_back(f);
                }
                Output::NeedMoreInput => return Ok(()),
                Output::StreamChange => state.out = negotiate_output(&state.mft, &self.stream)?,
            }
        }
    }
}

impl VideoDecoder for MfVideoDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.state()?;
        let data = match &self.annexb {
            Some(conv) => {
                self.scratch.clear();
                conv.convert(&packet.data, packet.keyframe, &mut self.scratch)?;
                &self.scratch[..]
            }
            None => &packet.data[..],
        };
        let sample = mft::sample(data, packet.pts, None)?;
        let mft = self.state.as_ref().unwrap().mft.clone();
        // SAFETY: COM call on a live transform.
        match unsafe { mft.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // Full: collect its output, then it takes the sample.
                self.pull()?;
                // SAFETY: as above.
                unsafe { mft.ProcessInput(0, &sample, 0) }.map_err(err("decode input"))
            }
            Err(e) => Err(err("decode input")(e)),
        }
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        if self.ready.is_empty()
            && let Some(state) = self.state.as_mut()
        {
            loop {
                match mft::process_output(&state.mft)? {
                    Output::Sample(s) => {
                        self.ready.push_back(read_frame(&s, &state.out)?);
                        break;
                    }
                    Output::NeedMoreInput => break,
                    Output::StreamChange => state.out = negotiate_output(&state.mft, &self.stream)?,
                }
            }
        }
        Ok(self.ready.pop_front().map(DecodedFrame::Yuv))
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

fn set_input(mft: &IMFTransform, subtype: GUID, stream: &StreamInfo) -> Result<()> {
    // SAFETY: COM calls on live objects.
    unsafe {
        let t = MFCreateMediaType().map_err(err("media type"))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(err("major type"))?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(err("subtype"))?;
        if stream.width > 0 && stream.height > 0 {
            t.SetUINT64(&MF_MT_FRAME_SIZE, (stream.width as u64) << 32 | stream.height as u64).map_err(err("frame size"))?;
        }
        mft.SetInputType(0, &t, 0).map_err(err("input type"))
    }
}

/// Picks NV12 (8-bit) or P010 (10-bit) output and reads its geometry and colour.
fn negotiate_output(mft: &IMFTransform, stream: &StreamInfo) -> Result<OutFormat> {
    // SAFETY: COM calls on live objects.
    unsafe {
        let mut chosen = None;
        for i in 0.. {
            let Ok(t) = mft.GetOutputAvailableType(0, i) else { break };
            let sub = t.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default();
            if sub == MFVideoFormat_NV12 {
                chosen = Some((t, false));
                break;
            }
            if sub == MFVideoFormat_P010 && chosen.is_none() {
                chosen = Some((t, true));
            }
        }
        let (t, p010) = chosen.ok_or_else(|| Error::Decode("Media Foundation offers no NV12/P010 output".into()))?;
        mft.SetOutputType(0, &t, 0).map_err(err("output type"))?;
        let size = t.GetUINT64(&MF_MT_FRAME_SIZE).map_err(err("output size"))?;
        let coded = ((size >> 32) as u32, size as u32);
        let mut visible = coded;
        let mut area = MFVideoArea::default();
        if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, std::slice::from_raw_parts_mut(&mut area as *mut _ as *mut u8, size_of::<MFVideoArea>()), None).is_ok()
            && area.Area.cx > 0
            && area.Area.cy > 0
        {
            visible = (area.Area.cx as u32, area.Area.cy as u32);
        } else if stream.width > 0 && stream.width <= coded.0 && stream.height <= coded.1 {
            visible = (stream.width, stream.height);
        }
        let matrix = match t.GetUINT32(&MF_MT_YUV_MATRIX).unwrap_or(0) {
            1 => Some(ColorMatrix::Bt709),
            2 => Some(ColorMatrix::Bt601),
            4 | 5 => Some(ColorMatrix::Bt2020),
            _ => None,
        };
        let full_range = t.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).unwrap_or(0) == MFNominalRange_0_255.0 as u32;
        Ok(OutFormat { coded, visible, p010, matrix, full_range })
    }
}

/// Copies an NV12/P010 output sample into an 8-bit I420 `YuvFrame`.
fn read_frame(sample: &IMFSample, out: &OutFormat) -> Result<YuvFrame> {
    // SAFETY: the buffer is locked while read and unlocked after; reads stay within the
    // locked pitch × coded-height layout the format describes.
    unsafe {
        let pts = mft::from_mf_time(sample.GetSampleTime().unwrap_or(0));
        let buffer = sample.ConvertToContiguousBuffer().map_err(err("output buffer"))?;
        let bps = if out.p010 { 2 } else { 1 };
        let (w, h) = out.visible;
        let (cw, ch) = chroma_size(PixelLayout::I420, w, h);
        let (mut y, mut u, mut v) = (vec![0u8; (w * h) as usize], vec![0u8; (cw * ch) as usize], vec![0u8; (cw * ch) as usize]);
        let mut read = |base: *const u8, pitch: usize| {
            let sample_at = |row: *const u8, i: usize| if out.p010 { *row.add(2 * i + 1) } else { *row.add(i) };
            for r in 0..h as usize {
                let row = base.add(r * pitch);
                for c in 0..w as usize {
                    y[r * w as usize + c] = sample_at(row, c);
                }
            }
            let uv = base.add(pitch * out.coded.1 as usize);
            for r in 0..ch as usize {
                let row = uv.add(r * pitch);
                for c in 0..cw as usize {
                    u[r * cw as usize + c] = sample_at(row, 2 * c);
                    v[r * cw as usize + c] = sample_at(row, 2 * c + 1);
                }
            }
        };
        if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
            let (mut ptr, mut pitch) = (std::ptr::null_mut(), 0i32);
            b2.Lock2D(&mut ptr, &mut pitch).map_err(err("lock"))?;
            read(ptr, pitch as usize);
            b2.Unlock2D().map_err(err("unlock"))?;
        } else {
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None).map_err(err("lock"))?;
            read(ptr, out.coded.0 as usize * bps);
            buffer.Unlock().map_err(err("unlock"))?;
        }
        Ok(YuvFrame {
            width: w,
            height: h,
            layout: PixelLayout::I420,
            planes: [y, u, v],
            strides: [w as usize, cw as usize, cw as usize],
            matrix: out.matrix.unwrap_or_else(|| ColorMatrix::guess_for_height(h)),
            full_range: out.full_range,
            pts,
        })
    }
}
