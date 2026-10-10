//! `AtAudioDecoder`: AudioToolbox's `AudioConverter`, compressed packets in, f32 out.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef, AudioConverterReset,
    AudioConverterSetProperty, kAudioConverterDecompressionMagicCookie,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};

use crate::apple::format::{AudioFormat, audio_format};
use crate::decode::{AudioBuffer as Samples, AudioDecoder, DelayTrim};
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

/// What our input callback answers once its one packet has been handed over.
const NO_MORE_INPUT: i32 = i32::from_be_bytes(*b"nodt");

/// The packet the converter is decoding, handed to it by `input`.
struct Input {
    data: *const u8,
    len: usize,
    channels: u32,
    served: bool,
    description: AudioStreamPacketDescription,
}

struct Converter {
    raw: AudioConverterRef,
    format: AudioFormat,
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: a live converter, disposed once.
        unsafe { AudioConverterDispose(self.raw) };
    }
}

pub struct AtAudioDecoder {
    stream: StreamInfo,
    converter: Option<Converter>,
    ready: VecDeque<Samples>,
    /// Encoder start-up padding (edit lists, CodecDelay) and end padding, where AudioToolbox does
    /// not drop them itself.
    trim: DelayTrim,
    /// Output buffer, reused.
    out: Vec<f32>,
}

// SAFETY: the converter is used only through `&mut self`, on the decode thread.
unsafe impl Send for AtAudioDecoder {}

impl AtAudioDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        Ok(Self {
            stream: stream.clone(),
            converter: None,
            ready: VecDeque::new(),
            trim: DelayTrim::new(absorbed_delay(stream)).with_end(stream.end_trim),
            out: Vec::new(),
        })
    }

    fn converter(&mut self, first_packet: &[u8]) -> Result<&mut Converter> {
        if self.converter.is_none() {
            let format = audio_format(&self.stream, first_packet).ok_or(Error::Unsupported("codec for AudioToolbox"))?;
            let input = AudioStreamBasicDescription {
                mSampleRate: format.rate as f64,
                mFormatID: format.id,
                mFormatFlags: format.format_flags,
                mBytesPerPacket: 0,
                mFramesPerPacket: format.frames_per_packet,
                mBytesPerFrame: 0,
                mChannelsPerFrame: format.channels,
                mBitsPerChannel: 0,
                mReserved: 0,
            };
            let output = AudioStreamBasicDescription {
                mSampleRate: format.rate as f64,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 4 * format.channels,
                mFramesPerPacket: 1,
                mBytesPerFrame: 4 * format.channels,
                mChannelsPerFrame: format.channels,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            let mut raw: AudioConverterRef = ptr::null_mut();
            // SAFETY: valid descriptions; the out-pointer receives a converter we own.
            let status = unsafe { AudioConverterNew(NonNull::from(&input), NonNull::from(&output), NonNull::from(&mut raw)) };
            if status != 0 || raw.is_null() {
                return Err(Error::Decode(format!("AudioConverterNew ({}): OSStatus {status}", fourcc(format.id))));
            }
            let converter = Converter { raw, format };
            if let Some(cookie) = &converter.format.cookie {
                // SAFETY: the cookie bytes outlive the call, which copies them.
                let status = unsafe {
                    AudioConverterSetProperty(
                        raw,
                        kAudioConverterDecompressionMagicCookie,
                        cookie.len() as u32,
                        NonNull::new_unchecked(cookie.as_ptr() as *mut c_void),
                    )
                };
                if status != 0 {
                    return Err(Error::Decode(format!("AudioToolbox refused the codec setup: OSStatus {status}")));
                }
            }
            log::info!("AudioToolbox {} {} Hz, {} channels", fourcc(converter.format.id), converter.format.rate, converter.format.channels);
            self.converter = Some(converter);
        }
        Ok(self.converter.as_mut().unwrap())
    }
}

impl AudioDecoder for AtAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.trim.on_packet(packet.pts);
        let conv = self.converter(&packet.data)?;
        let (raw, rate, channels) = (conv.raw, conv.format.rate, conv.format.channels);
        let capacity = conv.format.frames_per_packet.max(4096);
        let mut input = Input {
            data: packet.data.as_ptr(),
            len: packet.data.len(),
            channels,
            served: false,
            description: AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: 0 },
        };
        let mut produced = 0u64;
        loop {
            self.out.resize((capacity * channels) as usize, 0.0);
            let mut frames = capacity;
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer { mNumberChannels: channels, mDataByteSize: capacity * channels * 4, mData: self.out.as_mut_ptr().cast() }],
            };
            // SAFETY: `input` and `self.out` outlive the call; the callback hands over one packet.
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    raw,
                    Some(on_input),
                    &mut input as *mut Input as *mut c_void,
                    NonNull::from(&mut frames),
                    NonNull::from(&mut list),
                    ptr::null_mut(),
                )
            };
            if frames > 0 {
                let pts = packet.pts + std::time::Duration::from_secs_f64(produced as f64 / rate.max(1) as f64);
                produced += frames as u64;
                let samples = self.out[..(frames * channels) as usize].to_vec();
                if let Some(b) = self.trim.apply(samples, channels as u16, rate, pts) {
                    self.ready.push_back(b);
                }
            }
            match status {
                NO_MORE_INPUT => return Ok(()),
                0 if frames == 0 => return Ok(()),
                0 => {}
                s => return Err(Error::Decode(format!("AudioToolbox decode: OSStatus {s}"))),
            }
        }
    }

    fn receive_samples(&mut self) -> Result<Option<Samples>> {
        Ok(self.ready.pop_front())
    }

    fn flush(&mut self) {
        if let Some(c) = &self.converter {
            // SAFETY: a live converter.
            unsafe { AudioConverterReset(c.raw) };
        }
        self.ready.clear();
        self.trim.reset();
    }
}

/// The converter asks for input: our one packet, then "no more".
unsafe extern "C-unwind" fn on_input(
    _converter: AudioConverterRef,
    packets: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: `user` is the `Input` of the running fill call; the pointers come from the converter.
    unsafe {
        let input = &mut *(user as *mut Input);
        if input.served {
            *packets.as_ptr() = 0;
            return NO_MORE_INPUT;
        }
        input.served = true;
        let list = &mut *data.as_ptr();
        list.mNumberBuffers = 1;
        list.mBuffers[0] = AudioBuffer { mNumberChannels: input.channels, mDataByteSize: input.len as u32, mData: input.data as *mut c_void };
        *packets.as_ptr() = 1;
        if !descriptions.is_null() {
            input.description = AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: input.len as u32 };
            *descriptions = &mut input.description;
        }
        0
    }
}

/// The part of the stream's codec delay we must trim ourselves. AudioToolbox's AC-3 and E-AC-3
/// decoders hold back their 256-frame block overlap, which is exactly the encoder padding a
/// CodecDelay declares: their output already starts on the presentation timeline (measured
/// against ffmpeg, which drops the declared padding).
fn absorbed_delay(stream: &StreamInfo) -> std::time::Duration {
    use crate::demux::Codec;
    match stream.codec {
        Codec::Ac3 | Codec::Eac3 => std::time::Duration::ZERO,
        _ => stream.codec_delay,
    }
}

fn fourcc(id: u32) -> String {
    String::from_utf8_lossy(&id.to_be_bytes()).into_owned()
}
