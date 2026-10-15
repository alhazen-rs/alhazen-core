//! `AtAudioDecoder`: AudioToolbox's `AudioConverter`, compressed packets in, f32 out.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef, AudioConverterReset,
    AudioConverterSetProperty, kAudioConverterDecompressionMagicCookie, kAudioConverterOutputChannelLayout,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioChannelLayout, AudioChannelLayoutTag, AudioStreamBasicDescription,
    AudioStreamPacketDescription, kAudioChannelLayoutTag_WAVE_3_0, kAudioChannelLayoutTag_WAVE_4_0_B,
    kAudioChannelLayoutTag_WAVE_5_0_B, kAudioChannelLayoutTag_WAVE_5_1_A, kAudioChannelLayoutTag_WAVE_6_1,
    kAudioChannelLayoutTag_WAVE_7_1, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
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
    converter: Converter,
    /// E-AC-3's frames per packet come from its first syncframe: checked once, the converter
    /// rebuilt if it differs from the default.
    first_checked: bool,
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
    /// Fails when AudioToolbox refuses the stream, so the registry can try the next backend.
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let format = audio_format(stream, &[]).ok_or(Error::Unsupported("codec for AudioToolbox"))?;
        Ok(Self {
            stream: stream.clone(),
            converter: Converter::new(format)?,
            first_checked: false,
            ready: VecDeque::new(),
            trim: DelayTrim::new(absorbed_delay(stream)).with_end(stream.end_trim),
            out: Vec::new(),
        })
    }

    /// E-AC-3: rebuilds the converter if the first syncframe has fewer blocks than assumed.
    fn check_first_packet(&mut self, packet: &[u8]) -> Result<()> {
        if std::mem::replace(&mut self.first_checked, true) {
            return Ok(());
        }
        if let Some(format) = audio_format(&self.stream, packet)
            && format.frames_per_packet != self.converter.format.frames_per_packet
        {
            self.converter = Converter::new(format)?;
        }
        Ok(())
    }
}

impl Converter {
    fn new(format: AudioFormat) -> Result<Converter> {
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
        // The pipeline mixes WAVE order (FL FR FC LFE BL BR SL SR); AudioToolbox's Dolby decoders
        // otherwise hand out their own (L C R Ls Rs LFE): ask it to reorder.
        if let Some(tag) = wave_layout(converter.format.channels) {
            // SAFETY: a plain C struct; all-zero is a valid "no descriptions, no bitmap" layout.
            let mut layout: AudioChannelLayout = unsafe { std::mem::zeroed() };
            layout.mChannelLayoutTag = tag;
            // SAFETY: the layout outlives the call, which copies it.
            let status = unsafe {
                AudioConverterSetProperty(
                    raw,
                    kAudioConverterOutputChannelLayout,
                    size_of::<AudioChannelLayout>() as u32,
                    NonNull::from(&layout).cast(),
                )
            };
            if status != 0 {
                return Err(Error::Decode(format!("AudioToolbox cannot output WAVE channel order: OSStatus {status}")));
            }
        }
        log::info!("AudioToolbox {} {} Hz, {} channels", fourcc(converter.format.id), converter.format.rate, converter.format.channels);
        Ok(converter)
    }
}

impl AudioDecoder for AtAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.trim.on_packet(packet.pts);
        self.check_first_packet(&packet.data)?;
        let conv = &self.converter;
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
                s => {
                    // The converter may still hold on to this packet (and its description):
                    // reset it, so the next fill call cannot read them once they are gone.
                    // SAFETY: a live converter.
                    unsafe { AudioConverterReset(raw) };
                    return Err(Error::Decode(format!("AudioToolbox decode: OSStatus {s}")));
                }
            }
        }
    }

    fn receive_samples(&mut self) -> Result<Option<Samples>> {
        Ok(self.ready.pop_front())
    }

    fn flush(&mut self) {
        // SAFETY: a live converter.
        unsafe { AudioConverterReset(self.converter.raw) };
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

/// The WAVE-order layout the mixer expects for `channels` (none needed for mono and stereo).
fn wave_layout(channels: u32) -> Option<AudioChannelLayoutTag> {
    Some(match channels {
        3 => kAudioChannelLayoutTag_WAVE_3_0,
        4 => kAudioChannelLayoutTag_WAVE_4_0_B,
        5 => kAudioChannelLayoutTag_WAVE_5_0_B,
        6 => kAudioChannelLayoutTag_WAVE_5_1_A,
        7 => kAudioChannelLayoutTag_WAVE_6_1,
        8 => kAudioChannelLayoutTag_WAVE_7_1,
        _ => return None,
    })
}

fn fourcc(id: u32) -> String {
    String::from_utf8_lossy(&id.to_be_bytes()).into_owned()
}
