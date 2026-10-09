//! RIFF/WAVE: integer and float PCM (including WAVE_FORMAT_EXTENSIBLE), exact seeking, `LIST/INFO`
//! and `id3 ` tags.

use std::time::Duration;

use super::window::ReadWindow;
use super::{Codec, Demuxer, Metadata, Packet, PcmFormat, StreamInfo, StreamKind, tags};
use crate::source::MediaSource;
use crate::{Error, Result};

/// Sample frames per packet.
const PACKET_FRAMES: u64 = 2048;
/// Tag chunks larger than this are skipped.
const MAX_TAG_CHUNK: u64 = 32 << 20;

pub struct WavDemuxer {
    w: ReadWindow,
    streams: Vec<StreamInfo>,
    metadata: Option<Metadata>,
    data_start: u64,
    /// `u64::MAX` when the data runs to an unknown end (written while streaming).
    data_end: u64,
    block_align: u64,
    rate: u32,
    pos: u64,
}

impl WavDemuxer {
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut w = ReadWindow::new(src);
        let head = w.at(0, 12)?;
        if head.len() < 12 || &head[..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(Error::UnsupportedContainer);
        }
        let file_end = w.len().unwrap_or(u64::MAX);
        let mut meta = Metadata::default();
        let (mut fmt, mut data) = (None::<Vec<u8>>, None::<(u64, u64)>);
        let mut pos = 12u64;
        while pos.saturating_add(8) <= file_end {
            let chunk = w.at(pos, 8)?.to_vec();
            if chunk.len() < 8 {
                break;
            }
            let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
            let body = pos + 8;
            match &chunk[..4] {
                b"fmt " => fmt = Some(w.at(body, size.min(64) as usize)?.to_vec()),
                b"data" => {
                    // 0 or 0xFFFFFFFF: written while streaming; the data runs to the end of the file.
                    let end = if size == 0 || size == u32::MAX as u64 { file_end } else { (body + size).min(file_end) };
                    data = Some((body, end));
                    if end == file_end || !w.is_seekable() {
                        break; // nothing after it, or no cheap way past it
                    }
                }
                b"LIST" if (4..=MAX_TAG_CHUNK).contains(&size) => {
                    let list = w.at(body, size as usize)?.to_vec();
                    if let Some(info) = list.strip_prefix(b"INFO") {
                        tags::riff::parse_riff_info(info, &mut meta);
                    }
                }
                b"id3 " | b"ID3 " if size <= MAX_TAG_CHUNK => {
                    let tag = w.at(body, size as usize)?.to_vec();
                    tags::id3::parse_id3v2(&tag, &mut meta);
                }
                _ => {}
            }
            pos = body.saturating_add(size + (size & 1));
        }
        let fmt = fmt.ok_or_else(|| Error::Demux("wav: no fmt chunk".into()))?;
        let (data_start, data_end) = data.ok_or_else(|| Error::Demux("wav: no data chunk".into()))?;
        let (codec, channels, rate, block_align) = format(&fmt).ok_or_else(|| Error::Demux("wav: malformed fmt chunk".into()))?;
        let mut info = StreamInfo::new(0, StreamKind::Audio, codec);
        info.sample_rate = rate;
        info.channels = channels;
        if data_end != u64::MAX {
            let frames = (data_end - data_start) / block_align as u64;
            info.duration = Some(Duration::from_secs_f64(frames as f64 / rate as f64));
        }
        Ok(Self {
            w,
            streams: vec![info],
            metadata: (!meta.is_empty()).then_some(meta),
            data_start,
            data_end,
            block_align: block_align as u64,
            rate,
            pos: data_start,
        })
    }

    fn time(&self, frame: u64) -> Duration {
        Duration::from_secs_f64(frame as f64 / self.rate as f64)
    }
}

/// (codec, channels, sample rate, block align) from a `fmt ` chunk.
fn format(f: &[u8]) -> Option<(Codec, u16, u32, u16)> {
    let u16_at = |i: usize| Some(u16::from_le_bytes([*f.get(i)?, *f.get(i + 1)?]));
    let tag = u16_at(0)?;
    let channels = u16_at(2)?;
    let rate = u32::from_le_bytes(f.get(4..8)?.try_into().ok()?);
    let block_align = u16_at(12)?;
    let bits = u16_at(14)?;
    if channels == 0 || rate == 0 || block_align == 0 {
        return None;
    }
    // WAVE_FORMAT_EXTENSIBLE: the real format code starts the sub-format GUID.
    let code = if tag == 0xFFFE { u16_at(24)? } else { tag };
    let codec = match (code, bits) {
        (1, 8) => Codec::Pcm(PcmFormat::int(8, false, false)),
        (1, 16 | 24 | 32) => Codec::Pcm(PcmFormat::int(bits, false, true)),
        (3, 32 | 64) => Codec::Pcm(PcmFormat::float(bits, false)),
        _ => {
            let name = match code {
                0x0002 => " (MS ADPCM)",
                0x0006 => " (A-law)",
                0x0007 => " (µ-law)",
                0x0011 => " (IMA ADPCM)",
                0x0055 => " (MP3)",
                _ => "",
            };
            Codec::Other(format!("WAV format {code:#06x}{name}"))
        }
    };
    // For PCM the frame size follows from the layout; a wrong stated block_align would misplace
    // every packet.
    let block_align = match &codec {
        Codec::Pcm(p) => {
            let frame = channels as u32 * (p.bits as u32).div_ceil(8);
            if frame != block_align as u32 {
                log::warn!("wav: block_align {block_align} does not match the format; using {frame}");
            }
            u16::try_from(frame).ok()?
        }
        _ => block_align,
    };
    Some((codec, channels, rate, block_align))
}

impl Demuxer for WavDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let left = (self.data_end - self.pos) / self.block_align * self.block_align;
        if left == 0 {
            return Ok(None);
        }
        let want = left.min(PACKET_FRAMES * self.block_align) as usize;
        let bytes = self.w.at(self.pos, want)?;
        let whole = bytes.len() as u64 / self.block_align * self.block_align;
        if whole == 0 {
            return Ok(None); // end of a truncated file
        }
        let data = bytes[..whole as usize].to_vec();
        let pts = self.time((self.pos - self.data_start) / self.block_align);
        self.pos += whole;
        Ok(Some(Packet { stream: 0, pts, keyframe: true, data, generation: 0 }))
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        let frames = (self.data_end.saturating_sub(self.data_start)) / self.block_align;
        let frame = ((target.as_secs_f64() * self.rate as f64) as u64).min(frames);
        self.pos = self.data_start + frame * self.block_align;
        Ok(self.time(frame))
    }
}
