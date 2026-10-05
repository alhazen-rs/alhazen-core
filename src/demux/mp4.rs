//! MP4 / ISO-BMFF demuxer (indexes all samples up front via `re_mp4`).

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::time::Duration;

use super::{Codec, Demuxer, Packet, StreamInfo, StreamKind};
use crate::source::MediaSource;
use crate::{Error, Result};

struct SampleRef {
    stream: u32,
    offset: u64,
    size: u64,
    pts: Duration,
    keyframe: bool,
}

pub struct Mp4Demuxer {
    src: Box<dyn MediaSource>,
    streams: Vec<StreamInfo>,
    /// All samples of all tracks in file order.
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
                if let Some(esds) = &mp4a.esds {
                    let d = &esds.es_desc.dec_config.dec_specific;
                    info.extradata = Some(audio_specific_config(d.profile, d.freq_index, d.chan_conf));
                }
            }
            streams.push(info);
            samples.extend(track.samples.iter().map(|s| SampleRef {
                stream: track.track_id,
                offset: s.offset,
                size: s.size,
                pts: ticks(s.composition_timestamp, s.timescale.max(1)),
                keyframe: s.is_sync,
            }));
        }
        samples.sort_by_key(|s| s.offset);
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
    fn reads_aac_audio_track() {
        let d = Mp4Demuxer::open(Box::new(FileSource::open("tests/fixtures/aac_only.m4a").unwrap())).unwrap();
        let a = d.streams().iter().find(|s| s.kind == StreamKind::Audio).unwrap();
        assert_eq!(a.codec, Codec::Aac);
        assert_eq!((a.sample_rate, a.channels), (44_100, 1));
        assert_eq!(a.extradata.as_deref(), Some(&[0x12, 0x08][..]), "AAC-LC, 44.1 kHz, mono");
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
}
