//! AAC in ADTS frames (`.aac`), optionally after ID3v2 tags. Each packet is one raw AAC frame (the
//! ADTS header stripped); the AudioSpecificConfig is built from the first header.

use std::time::Duration;

use super::mpeg_audio::{AdtsHeader, Frames, find_chain, read_id3v1, read_id3v2_tags};
use super::window::ReadWindow;
use super::{Codec, Demuxer, Metadata, Packet, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

const FIRST_FRAME_WINDOW: usize = 64 * 1024;
/// AAC decoders need the previous frame's overlap (two frames also cover SBR's delay).
const PREROLL_FRAMES: u64 = 2;

pub struct AdtsDemuxer {
    frames: Frames<AdtsHeader>,
    streams: Vec<StreamInfo>,
    metadata: Option<Metadata>,
    rate: u32,
    /// Average frame size from the frames seen at open, for seeking without an index.
    bytes_per_frame: f64,
}

impl AdtsDemuxer {
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut w = ReadWindow::new(src);
        let mut meta = Metadata::default();
        let start = read_id3v2_tags(&mut w, &mut meta)?;
        let window = w.at(start, FIRST_FRAME_WINDOW)?.to_vec();
        let (i, h) = find_chain::<AdtsHeader>(&window, 2, None).ok_or(Error::Unsupported("no ADTS frames"))?;
        if h.blocks > 1 {
            // Block boundaries are only signalled with CRCs; ffmpeg doesn't support these either.
            return Err(Error::Unsupported("ADTS frames with several AAC blocks"));
        }
        let first = start + i as u64;
        // Average frame size over the frames chained in the first window.
        let (mut pos, mut count) = (i, 0u64);
        while let Some(f) = AdtsHeader::parse(&window[pos..]).filter(|f| pos + f.frame_len <= window.len()) {
            pos += f.frame_len;
            count += 1;
        }
        let bytes_per_frame = (pos - i) as f64 / count.max(1) as f64;
        let end = match read_id3v1(&mut w, &mut meta)? {
            Some(end) => end,
            None => w.len().unwrap_or(u64::MAX),
        };
        let mut frames = Frames::new(w, first, end, h);
        let total = if frames.w.is_local() {
            Some(frames.count()?)
        } else {
            (end != u64::MAX).then(|| ((end - first) as f64 / bytes_per_frame) as u64)
        };
        let rate = h.sample_rate;
        let mut s = StreamInfo::new(0, StreamKind::Audio, Codec::Aac);
        s.sample_rate = rate;
        // Channel configuration 7 is 7.1 (8 channels); 0 means "in the bitstream" (assume stereo).
        s.channels = match h.channel_config {
            0 => 2,
            7 => 8,
            c => c as u16,
        };
        s.extradata = Some(h.audio_specific_config());
        s.duration = total.map(|n| Duration::from_secs_f64((n * 1024) as f64 / rate as f64));
        s.seek_preroll = Duration::from_secs_f64((PREROLL_FRAMES * 1024) as f64 / rate as f64);
        Ok(Self { frames, streams: vec![s], metadata: (!meta.is_empty()).then_some(meta), rate, bytes_per_frame })
    }

    fn time(&self, frame: u64) -> Duration {
        Duration::from_secs_f64((frame * 1024) as f64 / self.rate as f64)
    }
}

impl Demuxer for AdtsDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let Some((n, h, frame)) = self.frames.next()? else { return Ok(None) };
        Ok(Some(Packet { stream: 0, pts: self.time(n), keyframe: true, data: frame[h.header_len..].to_vec(), generation: 0 }))
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        let frame = (target.as_secs_f64() * self.rate as f64 / 1024.0) as u64;
        let n = if self.frames.w.is_local() {
            self.frames.seek_exact(frame)?
        } else {
            let offset = self.frames.first + (frame as f64 * self.bytes_per_frame) as u64;
            self.frames.seek_approx(offset, frame)?
        };
        Ok(self.time(n))
    }
}
