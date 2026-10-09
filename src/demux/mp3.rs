//! MP3 files: MPEG audio Layer III frames after optional ID3v2 tags. A Xing/Info or VBRI header in
//! the first frame gives the frame count and seek table and is not audio; its LAME extension gives
//! the encoder delay and padding (gapless playback, as ffmpeg trims it).

use std::time::Duration;

use super::mpeg_audio::{Frames, MpegHeader, find_chain, read_id3v1, read_id3v2_tags};
use super::window::ReadWindow;
use super::{Codec, Demuxer, Metadata, Packet, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

/// The MP3 decoder's own delay, added to the LAME encoder delay (ffmpeg uses the same 529).
const DECODER_DELAY: u64 = 529;
/// Bytes searched for the first frame after the tags.
const FIRST_FRAME_WINDOW: usize = 64 * 1024;

/// What a Xing/Info (with LAME extension) or VBRI header says.
#[derive(Default)]
struct VbrInfo {
    frames: Option<u32>,
    bytes: Option<u32>,
    toc: Option<[u8; 100]>,
    delay: Option<u32>,
    padding: u32,
}

/// The VBR header in the first frame, if it has one.
fn vbr_info(frame: &[u8], h: &MpegHeader) -> Option<VbrInfo> {
    let at = 4 + h.side_info_len();
    let mut v = VbrInfo::default();
    if matches!(frame.get(at..at + 4), Some(b"Xing" | b"Info")) {
        let u32_at = |p: usize| Some(u32::from_be_bytes(frame.get(p..p + 4)?.try_into().ok()?));
        let flags = u32_at(at + 4)?;
        let mut p = at + 8;
        if flags & 1 != 0 {
            v.frames = u32_at(p);
            p += 4;
        }
        if flags & 2 != 0 {
            v.bytes = u32_at(p);
            p += 4;
        }
        if flags & 4 != 0 {
            v.toc = frame.get(p..p + 100).and_then(|t| t.try_into().ok());
            p += 100;
        }
        if flags & 8 != 0 {
            p += 4;
        }
        // LAME extension: a 9-byte encoder string ("LAME3.100", "Lavc61.19", …); 21 bytes in, the
        // 12-bit encoder delay and 12-bit end padding.
        if let Some(lame) = frame.get(p..p + 24)
            && lame[..4].iter().all(u8::is_ascii_alphanumeric)
        {
            v.delay = Some((lame[21] as u32) << 4 | (lame[22] as u32) >> 4);
            v.padding = ((lame[22] as u32) & 0xF) << 8 | lame[23] as u32;
        }
        return Some(v);
    }
    let at = 4 + 32; // VBRI sits at a fixed offset
    if frame.get(at..at + 4)? != b"VBRI" {
        return None;
    }
    v.bytes = Some(u32::from_be_bytes(frame.get(at + 10..at + 14)?.try_into().ok()?));
    v.frames = Some(u32::from_be_bytes(frame.get(at + 14..at + 18)?.try_into().ok()?));
    Some(v)
}

/// The Xing seek table: byte positions (in 1/256 of `bytes`, from `base`) at each 1 % of the
/// duration.
struct Toc {
    table: [u8; 100],
    bytes: u64,
    base: u64,
}

pub struct Mp3Demuxer {
    frames: Frames<MpegHeader>,
    streams: Vec<StreamInfo>,
    metadata: Option<Metadata>,
    spf: u64,
    rate: u32,
    toc: Option<Toc>,
    total_frames: Option<u64>,
    bytes_per_sec: f64,
}

impl Mp3Demuxer {
    pub fn open(src: Box<dyn MediaSource>) -> Result<Self> {
        let mut w = ReadWindow::new(src);
        let mut meta = Metadata::default();
        let start = read_id3v2_tags(&mut w, &mut meta)?;
        let window = w.at(start, FIRST_FRAME_WINDOW)?.to_vec();
        let (i, h) = find_chain::<MpegHeader>(&window, 2, None).ok_or(Error::Unsupported("no MPEG audio frames"))?;
        let header_frame = start + i as u64;
        let first_bytes = w.at(header_frame, h.frame_len)?.to_vec();
        let vbr = vbr_info(&first_bytes, &h);
        // The VBR header frame is not audio.
        let first = if vbr.is_some() { header_frame + h.frame_len as u64 } else { header_frame };
        let end = match read_id3v1(&mut w, &mut meta)? {
            Some(end) => end,
            None => w.len().unwrap_or(u64::MAX),
        };
        let mut frames = Frames::new(w, first, end, h);
        let vbr = vbr.unwrap_or_default();
        let total_frames = match vbr.frames {
            Some(n) => Some(n as u64),
            None if frames.w.is_local() => Some(frames.count()?),
            None => None,
        };
        let (rate, spf) = (h.sample_rate, h.samples as u64);
        let secs = |samples: u64| Duration::from_secs_f64(samples as f64 / rate as f64);
        let bytes_per_sec = h.bitrate_kbps as f64 * 125.0;
        let mut s = StreamInfo::new(0, StreamKind::Audio, Codec::Mp3);
        s.sample_rate = rate;
        s.channels = h.channels;
        if let Some(delay) = vbr.delay {
            s.codec_delay = secs(delay as u64 + DECODER_DELAY);
            if let Some(n) = total_frames {
                s.end_trim = Some(secs((n * spf).saturating_sub(delay as u64 + vbr.padding as u64)));
            }
        }
        s.duration = s
            .end_trim
            .or(total_frames.map(|n| secs(n * spf)))
            .or_else(|| (end != u64::MAX).then(|| Duration::from_secs_f64((end - first) as f64 / bytes_per_sec)));
        // The bit reservoir reaches up to 511 bytes back: decode enough frames before a seek target.
        s.seek_preroll = secs(spf * (1 + 511u64.div_ceil(h.frame_len as u64)));
        let toc = match (vbr.toc, vbr.bytes) {
            (Some(table), Some(bytes)) => Some(Toc { table, bytes: bytes as u64, base: header_frame }),
            _ => None,
        };
        Ok(Self { frames, streams: vec![s], metadata: (!meta.is_empty()).then_some(meta), spf, rate, toc, total_frames, bytes_per_sec })
    }

    fn time(&self, frame: u64) -> Duration {
        Duration::from_secs_f64((frame * self.spf) as f64 / self.rate as f64)
    }
}

impl Demuxer for Mp3Demuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let Some((n, _, data)) = self.frames.next()? else { return Ok(None) };
        Ok(Some(Packet { stream: 0, pts: self.time(n), keyframe: true, data, generation: 0 }))
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        let frame = (target.as_secs_f64() * self.rate as f64 / self.spf as f64) as u64;
        let n = if self.frames.w.is_local() {
            self.frames.seek_exact(frame)?
        } else {
            let offset = match (&self.toc, self.total_frames) {
                (Some(toc), Some(total)) if total > 0 => {
                    let pct = (frame as f64 / total as f64 * 100.0).clamp(0.0, 99.999);
                    let i = pct as usize;
                    let a = toc.table[i] as f64;
                    let b = if i < 99 { toc.table[i + 1] as f64 } else { 256.0 };
                    toc.base + ((a + (b - a) * (pct - i as f64)) / 256.0 * toc.bytes as f64) as u64
                }
                (_, Some(total)) if total > 0 && self.frames.end != u64::MAX => {
                    let audio = (self.frames.end - self.frames.first) as f64;
                    self.frames.first + (audio * frame.min(total) as f64 / total as f64) as u64
                }
                _ => self.frames.first + (target.as_secs_f64() * self.bytes_per_sec) as u64,
            };
            self.frames.seek_approx(offset, frame)?
        };
        Ok(self.time(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xing_lame_header_gives_frames_toc_delay_and_padding() {
        let h = MpegHeader::parse(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
        frame.resize(4 + 32, 0);
        frame.extend(b"Info");
        frame.extend(7u32.to_be_bytes()); // frames, bytes, TOC
        frame.extend(300u32.to_be_bytes());
        frame.extend(125_000u32.to_be_bytes());
        frame.extend((0..100u32).map(|i| (i * 256 / 100) as u8));
        frame.extend(b"LAME3.100");
        frame.resize(frame.len() + 12, 0); // revision … bitrate
        frame.extend([0x24, 0x01, 0x20]); // delay 576 (0x240), padding 288 (0x120)
        frame.resize(h.frame_len, 0);
        let v = vbr_info(&frame, &h).unwrap();
        assert_eq!((v.frames, v.bytes, v.delay, v.padding), (Some(300), Some(125_000), Some(576), 288));
        assert_eq!(v.toc.unwrap()[50], 128);
    }

    /// In-memory bytes seen as a network source of unknown length.
    struct Unsized(std::io::Cursor<Vec<u8>>);

    impl std::io::Read for Unsized {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl std::io::Seek for Unsized {
        fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
            self.0.seek(to)
        }
    }

    impl MediaSource for Unsized {
        fn byte_len(&self) -> Option<u64> {
            None
        }
        fn is_seekable(&self) -> bool {
            true
        }
        fn is_live(&self) -> bool {
            false
        }
        fn description(&self) -> String {
            "unsized".into()
        }
    }

    #[test]
    fn seeking_past_the_end_of_a_stream_of_unknown_length() {
        // An Info frame with a frame count but no seek table, then the audio of a CBR file.
        let file = std::fs::read("tests/fixtures/mp3_no_xing.mp3").unwrap();
        let first = super::super::tags::id3::id3v2_len(&file).unwrap_or(0) as usize;
        let h = MpegHeader::parse(&file[first..]).unwrap();
        let mut info = file[first..first + 4].to_vec();
        info.resize(4 + h.side_info_len(), 0);
        info.extend(b"Info");
        info.extend(1u32.to_be_bytes()); // frames only
        info.extend(1000u32.to_be_bytes());
        info.resize(h.frame_len, 0);
        let bytes = [&info[..], &file[first..]].concat();
        let mut d = Mp3Demuxer::open(Box::new(Unsized(std::io::Cursor::new(bytes)))).unwrap();
        d.seek(Duration::from_secs(999)).unwrap();
        while d.next_packet().unwrap().is_some() {}
    }

    #[test]
    fn plain_audio_frames_have_no_vbr_header() {
        let h = MpegHeader::parse(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
        frame.resize(h.frame_len, 0x55);
        assert!(vbr_info(&frame, &h).is_none());
    }
}
