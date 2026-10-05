//! Shared plumbing for Media Foundation decoder transforms (synchronous MFTs).

use std::time::Duration;

use windows::Win32::Media::MediaFoundation::*;
use windows::core::{GUID, Interface};

use crate::{Error, Result};

/// A Media Foundation error as ours.
pub fn err(what: &str) -> impl Fn(windows::core::Error) -> Error + '_ {
    move |e| Error::Decode(format!("Media Foundation {what}: {e}"))
}

/// The first installed synchronous decoder transform taking `subtype` in `category` and
/// producing one of `outputs` (tried in order). Filtering on the output matters: Windows also
/// registers, e.g., a Dolby AC-3 → S/PDIF passthrough converter that takes AC-3 but decodes
/// nothing.
pub fn find_decoder(category: GUID, major: GUID, subtype: GUID, outputs: &[GUID]) -> Option<IMFActivate> {
    outputs.iter().find_map(|&out| find_one(category, MFT_REGISTER_TYPE_INFO { guidMajorType: major, guidSubtype: subtype }, MFT_REGISTER_TYPE_INFO { guidMajorType: major, guidSubtype: out }))
}

fn find_one(category: GUID, input: MFT_REGISTER_TYPE_INFO, output: MFT_REGISTER_TYPE_INFO) -> Option<IMFActivate> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: valid in/out pointers; the returned array is CoTaskMemAlloc'd and freed below
    // after taking ownership of every element.
    unsafe {
        let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
        MFTEnumEx(category, flags, Some(&input), Some(&output), &mut activates, &mut count).ok()?;
        if activates.is_null() {
            return None;
        }
        let all: Vec<Option<IMFActivate>> = (0..count as usize).map(|i| std::ptr::read(activates.add(i))).collect();
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        all.into_iter().flatten().next()
    }
}

/// Wraps `data` in a sample with time `pts` (and `duration`, for decoders that need one).
pub fn sample(data: &[u8], pts: Duration, duration: Option<Duration>) -> Result<IMFSample> {
    // SAFETY: the buffer is created with `data.len()` capacity, locked for the copy, and
    // unlocked before it is attached to the sample.
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len().max(1) as u32).map_err(err("buffer"))?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None).map_err(err("lock"))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().map_err(err("unlock"))?;
        buffer.SetCurrentLength(data.len() as u32).map_err(err("length"))?;
        let sample = MFCreateSample().map_err(err("sample"))?;
        sample.AddBuffer(&buffer).map_err(err("add buffer"))?;
        sample.SetSampleTime(to_mf_time(pts)).map_err(err("time"))?;
        if let Some(d) = duration {
            sample.SetSampleDuration(to_mf_time(d)).map_err(err("duration"))?;
        }
        Ok(sample)
    }
}

pub fn to_mf_time(t: Duration) -> i64 {
    (t.as_nanos() / 100) as i64
}

pub fn from_mf_time(t: i64) -> Duration {
    Duration::from_nanos(t.max(0) as u64 * 100)
}

/// One `ProcessOutput` call's result.
pub enum Output {
    Sample(IMFSample),
    NeedMoreInput,
    /// The output format changed (e.g. resolution) or was dropped: renegotiate, then call again.
    StreamChange,
}

/// Output buffer size when a decoder reports none (0): fits any compressed-audio frame's PCM.
const MIN_OUTPUT_BUFFER: u32 = 1 << 20;

/// Pulls one output sample, allocating it when the transform does not provide its own.
pub fn process_output(mft: &IMFTransform) -> Result<Output> {
    // SAFETY: COM calls on a live transform; the output buffer struct owns the sample it
    // receives (ManuallyDrop fields are taken back into owned values below).
    unsafe {
        let info = mft.GetOutputStreamInfo(0).map_err(err("output info"))?;
        let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let mut buffer = MFT_OUTPUT_DATA_BUFFER::default();
        if !provides {
            let mem = MFCreateMemoryBuffer(info.cbSize.max(MIN_OUTPUT_BUFFER)).map_err(err("output buffer"))?;
            let s = MFCreateSample().map_err(err("output sample"))?;
            s.AddBuffer(&mem).map_err(err("add output buffer"))?;
            buffer.pSample = std::mem::ManuallyDrop::new(Some(s));
        }
        let mut status = 0u32;
        let r = mft.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status);
        let sample = std::mem::ManuallyDrop::take(&mut buffer.pSample);
        drop(std::mem::ManuallyDrop::take(&mut buffer.pEvents));
        match r {
            Ok(()) => sample.map(Output::Sample).ok_or_else(|| Error::Decode("Media Foundation gave no sample".into())),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::NeedMoreInput),
            // The Dolby decoders drop their output type once they have parsed the stream and
            // answer TYPE_NOT_SET instead of STREAM_CHANGE: both mean "pick an output type again".
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE || e.code() == MF_E_TRANSFORM_TYPE_NOT_SET => {
                Ok(Output::StreamChange)
            }
            Err(e) => Err(err("decode")(e)),
        }
    }
}

/// The contiguous bytes of `sample` (copied).
pub fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: the buffer is locked for the copy and unlocked after.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(err("buffer"))?;
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut ptr, None, Some(&mut len)).map_err(err("lock"))?;
        let v = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock().map_err(err("unlock"))?;
        Ok(v)
    }
}

/// Drops every pending sample and tells the transform a new stream starts (seek).
pub fn flush(mft: &IMFTransform) {
    // SAFETY: COM calls on a live transform.
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
    }
}

/// End of input: the transform outputs what it holds, then reports NeedMoreInput.
pub fn drain(mft: &IMFTransform) {
    // SAFETY: COM calls on a live transform.
    unsafe {
        let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
    }
}

pub fn begin_streaming(mft: &IMFTransform) -> Result<()> {
    // SAFETY: COM calls on a live transform.
    unsafe {
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(err("begin streaming"))?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(err("start of stream"))
    }
}

/// The transform as an activated object.
pub fn activate(a: &IMFActivate) -> Result<IMFTransform> {
    // SAFETY: COM call on a live activation object.
    unsafe { a.ActivateObject::<IMFTransform>() }.map_err(err("activate"))
}

/// A friendly name for logs and `mf-check` (`MFT_FRIENDLY_NAME_Attribute`).
pub fn friendly_name(a: &IMFActivate) -> String {
    // SAFETY: COM calls; the string is freed by windows-rs.
    unsafe {
        let mut len = 0;
        if a.GetStringLength(&MFT_FRIENDLY_NAME_Attribute).map(|l| len = l).is_err() {
            return "unknown".into();
        }
        let mut buf = vec![0u16; len as usize + 1];
        match a.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None) {
            Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
            Err(_) => "unknown".into(),
        }
    }
}

/// Makes a transform hand GPU-backed samples: D3D11-aware transforms get the device manager.
pub fn use_gpu(mft: &IMFTransform, manager: &IMFDXGIDeviceManager) -> bool {
    // SAFETY: COM calls on live objects; the manager pointer is passed as the message param,
    // as `MFT_MESSAGE_SET_D3D_MANAGER` requires, and the transform AddRefs it.
    unsafe {
        let aware = mft.GetAttributes().ok().and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE).ok()).unwrap_or(0) != 0;
        aware && mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).is_ok()
    }
}
