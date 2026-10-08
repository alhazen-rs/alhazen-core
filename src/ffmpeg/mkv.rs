//! A minimal streaming Matroska writer: how packets are handed to ffmpeg on stdin.
//!
//! Layout: EBML header, unknown-size Segment, Info (1 ms timestamps), one TrackEntry, then
//! unknown-size Clusters of SimpleBlocks. Packets keep their own (presentation) timestamps.

use std::time::Duration;

use crate::demux::{Codec, StreamInfo, StreamKind};

/// Matroska CodecID for `stream`, if ffmpeg can be fed this codec through Matroska.
pub fn matroska_codec_id(stream: &StreamInfo) -> Option<String> {
    Some(
        match &stream.codec {
            Codec::Av1 => "V_AV1",
            Codec::Vp8 => "V_VP8",
            Codec::Vp9 => "V_VP9",
            Codec::H264 => "V_MPEG4/ISO/AVC",
            Codec::Hevc => "V_MPEGH/ISO/HEVC",
            Codec::ProRes => "V_PRORES",
            Codec::Opus => "A_OPUS",
            Codec::Vorbis => "A_VORBIS",
            Codec::Aac => "A_AAC",
            Codec::Mp3 => "A_MPEG/L3",
            Codec::Ac3 => "A_AC3",
            Codec::Eac3 => "A_EAC3",
            Codec::Flac => "A_FLAC",
            Codec::Alac => "A_ALAC",
            // Matroska sources keep the original CodecID for codecs we don't model.
            Codec::Other(id) if id.starts_with("V_") || id.starts_with("A_") => id,
            // Native PCM always decodes it; the header writer has no BitDepth element.
            Codec::Pcm(_) | Codec::Other(_) => return None,
        }
        .to_owned(),
    )
}

/// CodecPrivate to send: the stream's extradata where Matroska and MP4 agree on its format
/// (`avcC`, `hvcC`, `av1C`, AudioSpecificConfig, Xiph headers, OpusHead), none otherwise.
fn codec_private(stream: &StreamInfo) -> Option<&[u8]> {
    match stream.codec {
        // MP4 `vpcC` is not Matroska's VP9 CodecPrivate; neither codec needs one.
        Codec::Vp8 | Codec::Vp9 => None,
        _ => stream.extradata.as_deref(),
    }
}

/// Writes the stream header (everything before the first Cluster) for a single-track file.
pub fn header(stream: &StreamInfo) -> Option<Vec<u8>> {
    let codec_id = matroska_codec_id(stream)?;
    let mut ebml = Vec::new();
    element(&mut ebml, &[0x42, 0x86], &uint(1)); // EBMLVersion
    element(&mut ebml, &[0x42, 0xF7], &uint(1)); // EBMLReadVersion
    element(&mut ebml, &[0x42, 0xF2], &uint(4)); // EBMLMaxIDLength
    element(&mut ebml, &[0x42, 0xF3], &uint(8)); // EBMLMaxSizeLength
    element(&mut ebml, &[0x42, 0x82], b"matroska"); // DocType
    element(&mut ebml, &[0x42, 0x87], &uint(4)); // DocTypeVersion
    element(&mut ebml, &[0x42, 0x85], &uint(2)); // DocTypeReadVersion
    let mut out = Vec::new();
    element(&mut out, &[0x1A, 0x45, 0xDF, 0xA3], &ebml);
    out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]); // Segment
    out.extend_from_slice(&UNKNOWN_SIZE);

    let mut info = Vec::new();
    element(&mut info, &[0x2A, 0xD7, 0xB1], &uint(1_000_000)); // TimestampScale: 1 ms
    element(&mut info, &[0x4D, 0x80], b"video-core"); // MuxingApp
    element(&mut info, &[0x57, 0x41], b"video-core"); // WritingApp
    element(&mut out, &[0x15, 0x49, 0xA9, 0x66], &info);

    let mut track = Vec::new();
    element(&mut track, &[0xD7], &uint(1)); // TrackNumber
    element(&mut track, &[0x73, 0xC5], &uint(1)); // TrackUID
    let video = stream.kind == StreamKind::Video;
    element(&mut track, &[0x83], &uint(if video { 1 } else { 2 })); // TrackType
    element(&mut track, &[0x86], codec_id.as_bytes()); // CodecID
    if let Some(private) = codec_private(stream) {
        element(&mut track, &[0x63, 0xA2], private); // CodecPrivate
    }
    if stream.codec_delay > Duration::ZERO {
        element(&mut track, &[0x56, 0xAA], &uint(stream.codec_delay.as_nanos() as u64)); // CodecDelay
    }
    if video {
        let mut v = Vec::new();
        element(&mut v, &[0xB0], &uint(stream.width.max(1) as u64)); // PixelWidth
        element(&mut v, &[0xBA], &uint(stream.height.max(1) as u64)); // PixelHeight
        element(&mut track, &[0xE0], &v);
    } else {
        let mut a = Vec::new();
        let rate = if stream.sample_rate > 0 { stream.sample_rate } else { 48_000 };
        element(&mut a, &[0xB5], &(rate as f64).to_be_bytes()); // SamplingFrequency
        element(&mut a, &[0x9F], &uint(stream.channels.max(1) as u64)); // Channels
        element(&mut track, &[0xE1], &a);
    }
    let mut tracks = Vec::new();
    element(&mut tracks, &[0xAE], &track); // TrackEntry
    element(&mut out, &[0x16, 0x54, 0xAE, 0x6B], &tracks);
    Some(out)
}

/// Turns packets into Clusters/SimpleBlocks for track 1.
#[derive(Default)]
pub struct BlockWriter {
    cluster_ms: Option<i64>,
}

impl BlockWriter {
    pub fn block(&mut self, pts: Duration, keyframe: bool, data: &[u8]) -> Vec<u8> {
        let ms = pts.as_millis() as i64;
        let mut out = Vec::with_capacity(data.len() + 32);
        // SimpleBlock timestamps are i16 relative to the cluster; B-frames make them go backwards.
        let start_new = match self.cluster_ms {
            Some(c) => (ms - c).abs() > 30_000,
            None => true,
        };
        if start_new {
            out.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75]); // Cluster
            out.extend_from_slice(&UNKNOWN_SIZE);
            element(&mut out, &[0xE7], &uint(ms.max(0) as u64)); // Timestamp
            self.cluster_ms = Some(ms.max(0));
        }
        let rel = (ms - self.cluster_ms.unwrap_or(0)) as i16;
        let mut block = Vec::with_capacity(data.len() + 4);
        block.push(0x81); // track number 1 as a 1-byte vint
        block.extend_from_slice(&rel.to_be_bytes());
        block.push(if keyframe { 0x80 } else { 0x00 });
        block.extend_from_slice(data);
        element(&mut out, &[0xA3], &block); // SimpleBlock
        out
    }
}

const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

fn element(out: &mut Vec<u8>, id: &[u8], payload: &[u8]) {
    out.extend_from_slice(id);
    let len = payload.len() as u64;
    // Smallest vint that holds `len` (all-ones values are reserved for "unknown").
    let n = (1..=8).find(|&n| len < (1u64 << (7 * n)) - 1).unwrap_or(8);
    let marked = len | (1u64 << (7 * n));
    out.extend_from_slice(&marked.to_be_bytes()[8 - n..]);
    out.extend_from_slice(payload);
}

fn uint(v: u64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|&&b| b == 0).count();
    bytes[skip..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Demuxer, MatroskaDemuxer};
    use crate::source::MediaSource;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct Mem(Cursor<Vec<u8>>);
    impl Read for Mem {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Seek for Mem {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(p)
        }
    }
    impl MediaSource for Mem {
        fn byte_len(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
        fn is_seekable(&self) -> bool {
            true
        }
        fn is_live(&self) -> bool {
            false
        }
        fn description(&self) -> String {
            "memory".into()
        }
    }

    #[test]
    fn element_sizes_use_the_smallest_vint() {
        let mut out = vec![];
        element(&mut out, &[0xA3], &[7; 3]);
        assert_eq!(out, [0xA3, 0x83, 7, 7, 7]);
        let mut out = vec![];
        element(&mut out, &[0xA3], &[0; 200]);
        assert_eq!(&out[..3], &[0xA3, 0x40, 200]);
        assert_eq!(uint(0), [0]);
        assert_eq!(uint(1_000_000), [0x0F, 0x42, 0x40]);
    }

    #[test]
    fn round_trips_through_our_matroska_demuxer() {
        let mut stream = StreamInfo::new(9, StreamKind::Video, Codec::H264);
        (stream.width, stream.height) = (320, 240);
        stream.extradata = Some(vec![1, 2, 3, 4]);
        let mut bytes = header(&stream).unwrap();
        let mut w = BlockWriter::default();
        // Out-of-order pts (B-frames) and a jump that forces a second cluster.
        let pts = [0u64, 66, 33, 100, 40_000, 40_033];
        for (i, ms) in pts.iter().enumerate() {
            bytes.extend(w.block(Duration::from_millis(*ms), i == 0 || i == 4, &[i as u8; 10]));
        }
        let mut d = MatroskaDemuxer::open(Box::new(Mem(Cursor::new(bytes)))).unwrap();
        let s = &d.streams()[0];
        assert_eq!((s.kind, &s.codec, s.width, s.height), (StreamKind::Video, &Codec::H264, 320, 240));
        assert_eq!(s.extradata.as_deref(), Some(&[1, 2, 3, 4][..]));
        let packets: Vec<_> = std::iter::from_fn(|| d.next_packet().unwrap()).collect();
        assert_eq!(packets.iter().map(|p| p.pts.as_millis() as u64).collect::<Vec<_>>(), pts);
        assert_eq!(packets.iter().map(|p| p.keyframe).collect::<Vec<_>>(), [true, false, false, false, true, false]);
        assert!(packets.iter().enumerate().all(|(i, p)| p.data == [i as u8; 10]));
    }

    #[test]
    fn audio_track_carries_rate_and_channels() {
        let mut stream = StreamInfo::new(2, StreamKind::Audio, Codec::Aac);
        (stream.sample_rate, stream.channels) = (44_100, 2);
        let mut bytes = header(&stream).unwrap();
        bytes.extend(BlockWriter::default().block(Duration::ZERO, true, &[1, 2]));
        let d = MatroskaDemuxer::open(Box::new(Mem(Cursor::new(bytes)))).unwrap();
        let s = &d.streams()[0];
        assert_eq!((s.kind, &s.codec, s.sample_rate, s.channels), (StreamKind::Audio, &Codec::Aac, 44_100, 2));
    }

    #[test]
    fn unknown_mp4_codecs_have_no_matroska_id() {
        let s = StreamInfo::new(1, StreamKind::Video, Codec::Other("mp4v.20".into()));
        assert!(matroska_codec_id(&s).is_none());
        let s = StreamInfo::new(1, StreamKind::Audio, Codec::Other("A_AC3".into()));
        assert_eq!(matroska_codec_id(&s).as_deref(), Some("A_AC3"));
    }
}
