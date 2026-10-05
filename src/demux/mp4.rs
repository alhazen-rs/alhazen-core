//! MP4 / ISO-BMFF demuxer (indexes all samples up front via `re_mp4`).

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::time::Duration;

use super::{Codec, Demuxer, Packet, PcmFormat, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

struct SampleRef {
    stream: u32,
    offset: u64,
    size: u64,
    pts: Duration,
    /// Decode time: the order samples must be read in, whatever the file layout.
    dts: Duration,
    keyframe: bool,
}

pub struct Mp4Demuxer {
    src: Box<dyn MediaSource>,
    streams: Vec<StreamInfo>,
    /// All samples of all tracks in decode-time order.
    samples: Vec<SampleRef>,
    cursor: usize,
    video_track: Option<u32>,
}

impl Mp4Demuxer {
    pub fn open(mut src: Box<dyn MediaSource>) -> Result<Self> {
        if !src.is_seekable() {
            return Err(Error::Unsupported("MP4 from a non-seekable source"));
        }
        let size = src.byte_len().ok_or(Error::Unsupported("MP4 from a source of unknown length"))?;
        let mp4 = re_mp4::Mp4::read(BufReader::new(&mut src), size)
            .map_err(|e| Error::Demux(format!("mp4: {e}")))?;
        // re_mp4 keeps only three fields of the AAC config; read the real bytes ourselves.
        let moov = read_moov(src.as_mut(), size).unwrap_or_default();

        let mut streams = Vec::new();
        let mut samples = Vec::new();
        for track in mp4.tracks().values() {
            let kind = match track.kind {
                Some(re_mp4::TrackKind::Video) => StreamKind::Video,
                Some(re_mp4::TrackKind::Audio) => StreamKind::Audio,
                _ => StreamKind::Other,
            };
            let codec = track
                .codec_string(&mp4)
                .map(|s| Codec::from_mp4_codec_string(&s))
                .unwrap_or_else(|| Codec::Other("unknown".into()));
            let timescale = track.timescale.max(1);
            let mut info = StreamInfo::new(track.track_id, kind, codec);
            info.width = track.width as u32;
            info.height = track.height as u32;
            info.duration = Some(ticks(track.duration as i64, timescale));
            info.extradata = track.raw_codec_config(&mp4);
            if let re_mp4::StsdBoxContent::Mp4a(mp4a) = &track.trak(&mp4).mdia.minf.stbl.stsd.contents {
                // re_mp4 gives no codec string for mp4a; the sample entry itself means AAC.
                info.codec = Codec::Aac;
                info.sample_rate = mp4a.samplerate.value() as u32;
                info.channels = mp4a.channelcount;
                info.extradata = raw_audio_specific_config(&moov, track.track_id).or_else(|| {
                    mp4a.esds.as_ref().map(|esds| {
                        let d = &esds.es_desc.dec_config.dec_specific;
                        audio_specific_config(d.profile, d.freq_index, d.chan_conf)
                    })
                });
            }
            // re_mp4 only knows a few sample entries; QuickTime ProRes tracks come out as
            // "unknown" with no kind, so read the sample entry's FourCC ourselves.
            if matches!(info.codec, Codec::Other(_))
                && let Some(fourcc) = sample_entry_fourcc(&moov, track.track_id)
                && let Codec::ProRes = Codec::from_mp4_codec_string(&String::from_utf8_lossy(&fourcc))
            {
                info.codec = Codec::ProRes;
                info.kind = StreamKind::Video;
            }
            if matches!(info.codec, Codec::Other(_))
                && let Some((fourcc, entry)) = sample_entry(&moov, track.track_id)
                && let Some((format, channels, rate)) = pcm_entry(fourcc, entry)
            {
                info.codec = Codec::Pcm(format);
                info.kind = StreamKind::Audio;
                (info.channels, info.sample_rate) = (channels, rate);
            }
            // Tracks re_mp4 has no codec for still have a handler type: an audio track we cannot
            // decode should be reported as such, not silently ignored.
            if info.kind == StreamKind::Other {
                info.kind = match handler_type(&moov, track.track_id) {
                    Some(b"soun") => StreamKind::Audio,
                    Some(b"vide") => StreamKind::Video,
                    _ => StreamKind::Other,
                };
            }
            let track_samples = track.samples.iter().map(|s| SampleRef {
                stream: track.track_id,
                offset: s.offset,
                size: s.size,
                pts: ticks(s.composition_timestamp, s.timescale.max(1)),
                dts: ticks(s.decode_timestamp, s.timescale.max(1)),
                keyframe: s.is_sync,
            });
            match info.codec {
                // QuickTime stores PCM as one "sample" per audio frame (6 bytes for 24-bit stereo):
                // merge contiguous ones into packets of up to PCM_PACKET_FRAMES frames.
                Codec::Pcm(f) => {
                    let frame = (f.bits as u64 / 8 * info.channels as u64).max(1);
                    samples.extend(merge_contiguous(track_samples, frame * PCM_PACKET_FRAMES));
                }
                _ => samples.extend(track_samples),
            }
            streams.push(info);
        }
        let samples = interleave_by_time(samples);
        let video_track = streams.iter().find(|s| s.kind == StreamKind::Video).map(|s| s.id);
        Ok(Self { src, streams, samples, cursor: 0, video_track })
    }
}

impl Demuxer for Mp4Demuxer {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let Some(s) = self.samples.get(self.cursor) else {
            return Ok(None);
        };
        self.cursor += 1;
        let mut data = vec![0u8; s.size as usize];
        self.src.seek(SeekFrom::Start(s.offset))?;
        self.src.read_exact(&mut data)?;
        Ok(Some(Packet { stream: s.stream, pts: s.pts, keyframe: s.keyframe, data, generation: 0 }))
    }

    fn seek(&mut self, target: Duration) -> Result<Duration> {
        let video = self.video_track;
        let is_video_key = |s: &SampleRef| Some(s.stream) == video && s.keyframe;
        let index = self
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| is_video_key(s) && s.pts <= target)
            .max_by_key(|(_, s)| s.pts)
            .or_else(|| self.samples.iter().enumerate().find(|(_, s)| is_video_key(s)))
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.cursor = index;
        Ok(self.samples.get(index).map(|s| s.pts).unwrap_or_default())
    }
}

/// Audio frames per merged PCM packet.
const PCM_PACKET_FRAMES: u64 = 2048;

/// Merges runs of samples stored back to back into samples of at most `max_bytes`.
fn merge_contiguous(samples: impl Iterator<Item = SampleRef>, max_bytes: u64) -> Vec<SampleRef> {
    let mut out: Vec<SampleRef> = Vec::new();
    for s in samples {
        if let Some(last) = out.last_mut()
            && last.offset + last.size == s.offset
            && last.size + s.size <= max_bytes
        {
            last.size += s.size;
            continue;
        }
        out.push(s);
    }
    out
}

/// All samples of all tracks in decode-time order. Files usually store tracks interleaved, but
/// one that stores all video before all audio would otherwise starve the audio clock while video
/// back-pressure blocks demuxing.
fn interleave_by_time(mut samples: Vec<SampleRef>) -> Vec<SampleRef> {
    samples.sort_by_key(|s| (s.dts, s.stream));
    samples
}

/// The whole `moov` box (top-level boxes are walked by header, `mdat` is never read).
fn read_moov(src: &mut dyn MediaSource, size: u64) -> Option<Vec<u8>> {
    let mut pos = 0u64;
    while pos + 8 <= size {
        src.seek(SeekFrom::Start(pos)).ok()?;
        let mut head = [0u8; 16];
        src.read_exact(&mut head[..8]).ok()?;
        let mut len = u32::from_be_bytes(head[..4].try_into().ok()?) as u64;
        let mut header = 8;
        if len == 1 {
            src.read_exact(&mut head[8..16]).ok()?;
            len = u64::from_be_bytes(head[8..16].try_into().ok()?);
            header = 16;
        } else if len == 0 {
            len = size - pos;
        }
        if len < header {
            return None;
        }
        if &head[4..8] == b"moov" {
            let body = len - header;
            if body > 256 << 20 {
                return None;
            }
            let mut moov = vec![0u8; body as usize];
            src.read_exact(&mut moov).ok()?;
            return Some(moov);
        }
        pos += len;
    }
    None
}

/// Child boxes of `data` as (type, payload).
fn boxes(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let head = data.get(pos..pos + 8)?;
        let len = u32::from_be_bytes(head[..4].try_into().ok()?) as usize;
        let (start, end) = match len {
            1 => {
                let big = u64::from_be_bytes(data.get(pos + 8..pos + 16)?.try_into().ok()?) as usize;
                (pos + 16, pos.checked_add(big)?)
            }
            0 => (pos + 8, data.len()),
            n if n >= 8 => (pos + 8, pos + n),
            _ => return None,
        };
        let payload = data.get(start..end)?;
        let kind = &head[4..8];
        pos = end;
        Some((kind, payload))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| *k == kind).map(|(_, p)| p)
}

/// The sample entries (payload of `stsd` after version/flags and entry count) of `track_id`.
fn sample_entries(moov: &[u8], track_id: u32) -> Option<&[u8]> {
    let trak = trak(moov, track_id)?;
    let stsd = child(child(child(child(trak, b"mdia")?, b"minf")?, b"stbl")?, b"stsd")?;
    stsd.get(8..)
}

/// The `trak` box of `track_id`.
fn trak(moov: &[u8], track_id: u32) -> Option<&[u8]> {
    boxes(moov).filter(|(k, _)| *k == b"trak").map(|(_, p)| p).find(|trak| {
        child(trak, b"tkhd").and_then(|tkhd| {
            let at = if tkhd.first() == Some(&1) { 20 } else { 12 };
            Some(u32::from_be_bytes(tkhd.get(at..at + 4)?.try_into().ok()?))
        }) == Some(track_id)
    })
}

/// `track_id`'s handler type (`vide`, `soun`, …) from `mdia/hdlr`.
fn handler_type(moov: &[u8], track_id: u32) -> Option<&[u8; 4]> {
    let hdlr = child(child(trak(moov, track_id)?, b"mdia")?, b"hdlr")?;
    hdlr.get(8..12)?.try_into().ok()
}

/// `track_id`'s first sample entry as (FourCC, payload).
fn sample_entry(moov: &[u8], track_id: u32) -> Option<(&[u8], &[u8])> {
    boxes(sample_entries(moov, track_id)?).next()
}

/// PCM format, channels and rate of a QuickTime/ISO sound sample entry, if it is PCM.
fn pcm_entry(fourcc: &[u8], e: &[u8]) -> Option<(PcmFormat, u16, u32)> {
    let u16_at = |at: usize| Some(u16::from_be_bytes(e.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_be_bytes(e.get(at..at + 4)?.try_into().ok()?));
    // SoundDescription: reserved(6) data_ref(2) version(2) revision(2) vendor(4) channels(2)
    // sample_size(2) compression_id(2) packet_size(2) sample_rate(16.16); v1 adds 16 bytes,
    // v2 replaces rate/channels with its own fields (QuickTime File Format, "Sound Sample Descriptions").
    let version = u16_at(8)?;
    let (mut channels, sample_size, mut rate) = (u16_at(16)?, u16_at(18)?, u32_at(24)? >> 16);
    let children = match version {
        0 => 28,
        1 => 44,
        2 => 64,
        _ => return None,
    };
    let kids = e.get(children..).unwrap_or_default();
    // QuickTime: big-endian unless `wave/enda` says otherwise (as Final Cut / ffmpeg write `in24`).
    let little = child(kids, b"wave").and_then(|w| child(w, b"enda")).and_then(|v| v.get(..2)).is_some_and(|v| v != [0, 0]);
    let format = match fourcc {
        b"sowt" => PcmFormat::int(sample_size.max(16), false, true),
        b"twos" => PcmFormat::int(sample_size.max(8), true, true),
        b"raw " => PcmFormat::int(8, false, false),
        b"in24" => PcmFormat::int(24, !little, true),
        b"in32" => PcmFormat::int(32, !little, true),
        b"fl32" => PcmFormat::float(32, !little),
        b"fl64" => PcmFormat::float(64, !little),
        b"lpcm" if version == 2 => {
            rate = f64::from_bits(u64::from_be_bytes(e.get(32..40)?.try_into().ok()?)) as u32;
            channels = u32_at(40)? as u16;
            let (bits, flags) = (u32_at(48)? as u16, u32_at(52)?);
            match flags & 1 != 0 {
                true => PcmFormat::float(bits, flags & 2 != 0),
                false => PcmFormat::int(bits, flags & 2 != 0, flags & 4 != 0 || bits > 8),
            }
        }
        // ISO/IEC 23003-5: `pcmC` = version/flags(4), format_flags (bit 0: little endian), sample size.
        b"ipcm" | b"fpcm" => {
            let pcmc = child(kids, b"pcmC")?;
            let (flags, bits) = (*pcmc.get(4)?, *pcmc.get(5)? as u16);
            match fourcc == b"fpcm" {
                true => PcmFormat::float(bits, flags & 1 == 0),
                false => PcmFormat::int(bits, flags & 1 == 0, true),
            }
        }
        _ => return None,
    };
    let supported = if format.float { matches!(format.bits, 32 | 64) } else { matches!(format.bits, 8 | 16 | 24 | 32) };
    (supported && channels > 0 && rate > 0).then_some((format, channels, rate))
}

/// The FourCC of `track_id`'s first sample entry (e.g. `apch`).
fn sample_entry_fourcc(moov: &[u8], track_id: u32) -> Option<[u8; 4]> {
    let (kind, _) = boxes(sample_entries(moov, track_id)?).next()?;
    kind.try_into().ok()
}

/// The raw AudioSpecificConfig of `track_id`'s `mp4a` sample entry.
fn raw_audio_specific_config(moov: &[u8], track_id: u32) -> Option<Vec<u8>> {
    let mp4a = child(sample_entries(moov, track_id)?, b"mp4a")?;
    // AudioSampleEntry: 28 bytes of fields before its child boxes.
    parse_esds_asc(child(mp4a.get(28..)?, b"esds")?)
}

/// The DecSpecificInfo (AudioSpecificConfig) bytes inside an `esds` payload (ISO 14496-1).
fn parse_esds_asc(esds: &[u8]) -> Option<Vec<u8>> {
    /// Tag and expandable length (1–4 bytes, high bit = more) at `pos`; returns (tag, len, body start).
    fn descriptor(d: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
        let tag = *d.get(pos)?;
        let mut len = 0usize;
        let mut i = pos + 1;
        for _ in 0..4 {
            let b = *d.get(i)?;
            i += 1;
            len = (len << 7) | (b & 0x7F) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        d.get(i..i + len)?;
        Some((tag, len, i))
    }
    let (tag, _, mut pos) = descriptor(esds, 4)?; // after version/flags
    if tag != 0x03 {
        return None;
    }
    let flags = *esds.get(pos + 2)?;
    pos += 3; // ES_ID + flags
    if flags & 0x80 != 0 {
        pos += 2; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        pos += 1 + *esds.get(pos)? as usize; // URL
    }
    if flags & 0x20 != 0 {
        pos += 2; // OCR_ES_Id
    }
    let (tag, _, pos) = descriptor(esds, pos)?;
    if tag != 0x04 {
        return None;
    }
    let (tag, len, start) = descriptor(esds, pos + 13)?; // after the fixed DecoderConfig fields
    (tag == 0x05).then(|| esds[start..start + len].to_vec())
}

/// Rebuilds the 2-byte AAC AudioSpecificConfig (object type, frequency index, channel config).
fn audio_specific_config(profile: u8, freq_index: u8, chan_conf: u8) -> Vec<u8> {
    let v = ((profile as u16 & 0x1F) << 11) | ((freq_index as u16 & 0x0F) << 7) | ((chan_conf as u16 & 0x0F) << 3);
    v.to_be_bytes().to_vec()
}

fn ticks(t: i64, timescale: u64) -> Duration {
    let t = t.max(0) as u128;
    Duration::from_nanos((t * 1_000_000_000 / timescale as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileSource;

    fn open() -> Mp4Demuxer {
        Mp4Demuxer::open(Box::new(FileSource::open("tests/fixtures/av1.mp4").unwrap())).unwrap()
    }

    #[test]
    fn reads_stream_info_and_packets() {
        let mut d = open();
        let s = &d.streams()[0];
        assert_eq!((s.kind, s.codec.clone(), s.width, s.height), (StreamKind::Video, Codec::Av1, 320, 240));
        let mut n = 0;
        let mut first = None;
        while let Some(p) = d.next_packet().unwrap() {
            first.get_or_insert(p.keyframe);
            n += 1;
        }
        assert_eq!(n, 60);
        assert_eq!(first, Some(true));
    }

    #[test]
    fn quicktime_prores_track_is_video() {
        for name in ["prores_hq.mov", "prores_4444.mov"] {
            let src = Box::new(FileSource::open(format!("tests/fixtures/{name}")).unwrap());
            let mut d = Mp4Demuxer::open(src).unwrap();
            let s = &d.streams()[0];
            assert_eq!((s.kind, &s.codec, s.width, s.height), (StreamKind::Video, &Codec::ProRes, 192, 128), "{name}");
            let p = d.next_packet().unwrap().unwrap();
            assert_eq!(&p.data[4..8], b"icpf", "{name}: packets are whole ProRes frames");
        }
    }

    #[test]
    fn reads_aac_audio_track() {
        let d = Mp4Demuxer::open(Box::new(FileSource::open("tests/fixtures/aac_only.m4a").unwrap())).unwrap();
        let a = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap();
        assert_eq!(a.codec, Codec::Aac);
        assert_eq!((a.sample_rate, a.channels), (44_100, 1));
        // The file's own AudioSpecificConfig: AAC-LC, 44.1 kHz, mono, plus the explicit
        // "no SBR" extension (0x2B7 sync) that ffmpeg writes.
        assert_eq!(a.extradata.as_deref(), Some(&[0x12, 0x08, 0x56, 0xE5, 0x00][..]));
    }

    #[test]
    fn seek_lands_on_keyframe_at_or_before_target() {
        let mut d = open();
        let landed = d.seek(Duration::from_millis(1500)).unwrap();
        assert_eq!(landed, Duration::from_secs(1));
        let p = d.next_packet().unwrap().unwrap();
        assert!(p.keyframe);
        assert_eq!(p.pts, Duration::from_secs(1));
        assert_eq!(d.seek(Duration::from_millis(400)).unwrap(), Duration::ZERO);
    }

    #[test]
    fn packets_come_out_in_decode_order_even_if_stored_apart() {
        // All video stored before all audio in the file; reading must interleave by time,
        // or the audio clock starves while video back-pressure blocks demuxing.
        let s = |stream, offset, ms| SampleRef {
            stream,
            offset,
            size: 1,
            pts: Duration::from_millis(ms),
            dts: Duration::from_millis(ms),
            keyframe: true,
        };
        let ordered = interleave_by_time(vec![s(1, 0, 0), s(1, 10, 40), s(1, 20, 80), s(2, 100, 0), s(2, 110, 20), s(2, 120, 60)]);
        let order: Vec<(u32, u128)> = ordered.iter().map(|x| (x.stream, x.dts.as_millis())).collect();
        assert_eq!(order, [(1, 0), (2, 0), (2, 20), (1, 40), (2, 60), (1, 80)]);
    }

    #[test]
    fn esds_parser_returns_the_raw_audio_specific_config() {
        // Real payload from aac_only.m4a (4-byte expandable lengths, as ffmpeg writes them).
        let real = [
            0, 0, 0, 0, 0x03, 0x80, 0x80, 0x80, 0x25, 0x00, 0x01, 0x00, 0x04, 0x80, 0x80, 0x80, 0x17, 0x40, 0x15, 0, 0, 0,
            0, 0, 0xFD, 0xCD, 0, 0, 0xFD, 0xCD, 0x05, 0x80, 0x80, 0x80, 0x05, 0x12, 0x08, 0x56, 0xE5, 0x00, 0x06, 0x80,
            0x80, 0x80, 0x01, 0x02,
        ];
        assert_eq!(parse_esds_asc(&real), Some(vec![0x12, 0x08, 0x56, 0xE5, 0x00]));
        // Explicit sample rate (frequency index 15 + 24-bit rate), 1-byte lengths, URL flag set.
        let asc = [0x17, 0x80, 0x5D, 0xC0, 0x08];
        let mut esds = vec![0, 0, 0, 0, 0x03, 0];
        let es = {
            let mut es = vec![0x00, 0x01, 0x40, 3, b'a', b'b', b'c']; // ES_ID, flags (URL), url
            es.extend([0x04, 13 + 2 + asc.len() as u8, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            es.extend([0x05, asc.len() as u8]);
            es.extend(asc);
            es
        };
        esds[5] = es.len() as u8;
        esds.extend(es);
        assert_eq!(parse_esds_asc(&esds), Some(asc.to_vec()));
        assert_eq!(parse_esds_asc(&[0, 0, 0, 0, 0x03, 0x05, 0x00]), None, "truncated");
    }
}
