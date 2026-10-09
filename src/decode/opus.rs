//! Opus decoding via `opus-pure` (pure Rust, verified against C libopus 1.6.1).

use std::collections::VecDeque;
use std::time::Duration;

use opus_pure::OpusDecoder as Mono;

use super::channels::to_wave_order;
use super::opus_multistream::MultistreamDecoder;

use super::audio::{AudioBuffer, AudioDecoder};
use super::clip_end;
use crate::demux::{Packet, StreamInfo};
use crate::{Error, Result};

const RATE: u32 = 48_000;
/// Longest Opus packet: 120 ms at 48 kHz.
const MAX_FRAMES: usize = 5_760;

/// Parsed `OpusHead` (RFC 7845 §5.1).
#[derive(Debug, PartialEq)]
pub(crate) struct OpusHead {
    pub channels: u16,
    pub pre_skip: u16,
    /// Channel mapping family: 0 mono/stereo, 1 Vorbis-order surround, 2/3 ambisonics, 255 custom.
    pub family: u8,
    /// (stream count, coupled count, channel mapping) for mapping family != 0.
    pub multistream: Option<(u8, u8, Vec<u8>)>,
}

pub(crate) fn parse_opus_head(b: &[u8]) -> Option<OpusHead> {
    if b.len() < 19 || &b[..8] != b"OpusHead" {
        return None;
    }
    let channels = b[9] as u16;
    let pre_skip = u16::from_le_bytes([b[10], b[11]]);
    let family = b[18];
    let multistream = if family == 0 {
        None
    } else {
        let streams = *b.get(19)?;
        let coupled = *b.get(20)?;
        // RFC 7845 §5.1.1: at least one stream, coupled ≤ streams, streams + coupled ≤ 255.
        if streams == 0 || coupled > streams || streams as u16 + coupled as u16 > 255 {
            return None;
        }
        let mapping = b.get(21..21 + channels as usize)?.to_vec();
        Some((streams, coupled, mapping))
    };
    Some(OpusHead { channels, pre_skip, family, multistream })
}

enum Inner {
    /// Boxed: an Opus decoder's state is ~10 KB.
    Single(Box<Mono>),
    /// Surround: several streams per packet; output reordered to WAVE order.
    Multi(MultistreamDecoder),
}

pub struct OpusAudioDecoder {
    inner: Inner,
    channels: u16,
    pre_skip: usize,
    /// Frames still to discard (pre-skip at stream start).
    skip: usize,
    /// Matroska block times are offset by the codec delay (= pre-skip); presentation time is
    /// block time minus this, as ffmpeg's decoder reports it.
    codec_delay: Duration,
    /// The stream's exact presentation length (Ogg final granule): later samples are padding.
    end: Option<Duration>,
    /// Reorder surround output from Vorbis order to WAVE order (mapping family 1).
    reorder: bool,
    out: VecDeque<AudioBuffer>,
    pcm: Vec<f32>,
}

impl OpusAudioDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let head = stream.extradata.as_deref().and_then(parse_opus_head);
        let channels = head.as_ref().map(|h| h.channels).unwrap_or(stream.channels).max(1);
        let pre_skip = match &head {
            Some(h) => h.pre_skip as usize,
            None => (stream.codec_delay.as_nanos() * RATE as u128 / 1_000_000_000) as usize,
        };
        // Only family 1 uses the Vorbis channel order; 2/3 (ambisonics) and 255 stay as stored.
        let reorder = head.as_ref().is_some_and(|h| h.family == 1);
        let inner = match head.and_then(|h| h.multistream) {
            // Our own multistream layer: honors whatever stream counts and mapping OpusHead declares.
            Some((streams, coupled, mapping)) => Inner::Multi(
                MultistreamDecoder::new(RATE, channels as usize, streams as usize, coupled as usize, mapping)
                    .map_err(Error::Decode)?,
            ),
            None if channels <= 2 => {
                Inner::Single(Box::new(
                    Mono::new(RATE as i32, channels as usize).map_err(|e| Error::Decode(format!("opus init: {e}")))?,
                ))
            }
            None => return Err(Error::Decode(format!("opus: {channels} channels without a channel mapping"))),
        };
        Ok(Self {
            inner,
            channels,
            pre_skip,
            skip: pre_skip,
            codec_delay: stream.codec_delay,
            end: stream.end_trim,
            reorder,
            out: VecDeque::new(),
            pcm: vec![0.0; MAX_FRAMES * channels as usize],
        })
    }
}

impl AudioDecoder for OpusAudioDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.pts.is_zero() {
            // Stream start (also after seeking to 0): discard the encoder's pre-skip again.
            self.skip = self.pre_skip;
        }
        let frames = match &mut self.inner {
            Inner::Single(d) => d
                .decode(&packet.data, MAX_FRAMES, &mut self.pcm)
                .map_err(|e| Error::Decode(format!("opus: {e}")))?,
            Inner::Multi(d) => d.decode_float(&packet.data, &mut self.pcm).map_err(Error::Decode)?,
        };
        let ch = self.channels as usize;
        let drop = self.skip.min(frames);
        self.skip -= drop;
        if frames > drop {
            let samples = self.pcm[drop * ch..frames * ch].to_vec();
            let samples = match self.inner {
                Inner::Multi(_) if self.reorder => to_wave_order(samples, self.channels),
                _ => samples,
            };
            let pts = (packet.pts + Duration::from_nanos(drop as u64 * 1_000_000_000 / RATE as u64)).saturating_sub(self.codec_delay);
            let samples = clip_end(samples, self.channels, RATE, pts, self.end);
            if !samples.is_empty() {
                self.out.push_back(AudioBuffer { rate: RATE, channels: self.channels, samples, pts });
            }
        }
        Ok(())
    }

    fn receive_samples(&mut self) -> Result<Option<AudioBuffer>> {
        Ok(self.out.pop_front())
    }

    fn flush(&mut self) {
        self.out.clear();
        match &mut self.inner {
            Inner::Single(d) => {
                let _ = d.reset_state();
            }
            Inner::Multi(d) => d.reset(),
        }
        self.skip = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_opus_head() {
        let mut stereo = b"OpusHead".to_vec();
        stereo.extend([1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        assert_eq!(parse_opus_head(&stereo), Some(OpusHead { channels: 2, pre_skip: 312, family: 0, multistream: None }));
        let mut surround = b"OpusHead".to_vec();
        surround.extend([1, 6, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 1, 4, 2, 0, 4, 1, 2, 3, 5]);
        let head = parse_opus_head(&surround).unwrap();
        assert_eq!(head.multistream, Some((4, 2, vec![0, 4, 1, 2, 3, 5])));
        assert_eq!(parse_opus_head(b"OpusTags........."), None);
        assert_eq!(parse_opus_head(&surround[..20]), None, "truncated mapping");
    }

    #[test]
    fn rejects_impossible_stream_counts() {
        let head = |streams: u8, coupled: u8| {
            let mut h = b"OpusHead".to_vec();
            h.extend([1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 1, streams, coupled, 0, 1]);
            h
        };
        assert!(parse_opus_head(&head(1, 1)).is_some());
        assert_eq!(parse_opus_head(&head(200, 100)), None, "streams + coupled > 255");
        assert_eq!(parse_opus_head(&head(1, 2)), None, "more coupled streams than streams");
        assert_eq!(parse_opus_head(&head(0, 0)), None, "no streams");
    }
}
