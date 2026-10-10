//! What Apple's decoders need to know about a stream: codec identifiers, setup records built from
//! the first keyframe where the container has none, audio formats and cookies.

use crate::demux::{Codec, StreamInfo};

const fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

/// The `CMVideoCodecType` VideoToolbox decodes `codec` as (ProRes: from its first frame, see
/// [`prores_subtype`]).
pub(crate) fn video_codec_type(codec: &Codec) -> Option<u32> {
    Some(match codec {
        Codec::H264 => fourcc(b"avc1"),
        Codec::Hevc => fourcc(b"hvc1"),
        Codec::Av1 => fourcc(b"av01"),
        Codec::Vp9 => fourcc(b"vp09"),
        _ => return None,
    })
}

/// The sample description extension atom carrying the codec's setup record.
pub(crate) fn atom_name(codec: &Codec) -> Option<&'static str> {
    Some(match codec {
        Codec::H264 => "avcC",
        Codec::Hevc => "hvcC",
        Codec::Av1 => "av1C",
        Codec::Vp9 => "vpcC",
        _ => return None,
    })
}

/// MSB-first bit reader.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |v, _| {
            let bit = (self.data.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Some(v << 1 | bit as u32)
        })
    }
}

/// A VP9 `vpcC` record (FullBox version 1) from a keyframe's uncompressed header: profile, bit
/// depth, chroma subsampling, range and colour space. `None` if `frame` is not a keyframe.
pub(crate) fn vpcc_from_keyframe(frame: &[u8]) -> Option<Vec<u8>> {
    let mut b = Bits { data: frame, pos: 0 };
    if b.u(2)? != 2 {
        return None; // frame_marker
    }
    let low = b.u(1)?;
    let profile = (b.u(1)? << 1) | low;
    if profile == 3 {
        b.u(1)?;
    }
    if b.u(1)? == 1 || b.u(1)? != 0 {
        return None; // show_existing_frame, or not a keyframe
    }
    b.u(2)?; // show_frame, error_resilient_mode
    if b.u(24)? != 0x49_83_42 {
        return None;
    }
    let bit_depth = if profile >= 2 { if b.u(1)? == 1 { 12 } else { 10 } } else { 8 };
    let color_space = b.u(3)?;
    let (full_range, subsampling_x, subsampling_y) = if color_space != 7 {
        let range = b.u(1)?;
        if profile == 1 || profile == 3 { (range, b.u(1)?, b.u(1)?) } else { (range, 1, 1) }
    } else {
        (1, 0, 0) // sRGB: 4:4:4, full range
    };
    // VP9 chroma subsampling codes: 1 = 4:2:0 (co-located with luma), 2 = 4:2:2, 3 = 4:4:4.
    let chroma = match (subsampling_x, subsampling_y) {
        (1, 1) => 1,
        (1, 0) => 2,
        _ => 3,
    };
    // ITU-T H.273 primaries, transfer, matrix for the VP9 colour space.
    let (primaries, transfer, matrix) = match color_space {
        1 | 3 => (6, 6, 6),   // BT.601, SMPTE 170M
        2 => (1, 1, 1),       // BT.709
        4 => (7, 7, 7),       // SMPTE 240M
        5 => (9, 14, 9),      // BT.2020
        7 => (1, 13, 0),      // sRGB
        _ => (2, 2, 2),       // unknown
    };
    Some(vec![
        1, 0, 0, 0, // version 1, flags
        profile as u8,
        62, // level: the highest, so no stream is refused for its level
        ((bit_depth as u8) << 4) | ((chroma as u8) << 1) | full_range as u8,
        primaries,
        transfer,
        matrix,
        0,
        0, // no codec initialization data
    ])
}

/// The ProRes `CMVideoCodecType` for a frame: 4:4:4 frames as ProRes 4444, 4:2:2 ones as ProRes
/// 422 (the decoder handles every 422 variant under that type).
pub(crate) fn prores_subtype(frame: &[u8]) -> Option<u32> {
    if frame.get(4..8)? != b"icpf" {
        return None;
    }
    match frame.get(20)? >> 6 {
        2 => Some(fourcc(b"apcn")),
        3 => Some(fourcc(b"ap4h")),
        _ => None,
    }
}

/// An MPEG-4 descriptor: tag, 4-byte expandable length, body.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len() as u32;
    let mut d = vec![tag, 0x80 | (n >> 21 & 0x7F) as u8, 0x80 | (n >> 14 & 0x7F) as u8, 0x80 | (n >> 7 & 0x7F) as u8, (n & 0x7F) as u8];
    d.extend_from_slice(body);
    d
}

/// The MPEG-4 ES_Descriptor (the `esds` box's content after version and flags) for an
/// AudioSpecificConfig: AudioToolbox's magic cookie for AAC-family streams.
pub(crate) fn esds_from_asc(asc: &[u8]) -> Vec<u8> {
    let mut config = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // MPEG-4 audio, audio stream
    config.extend(descriptor(0x05, asc));
    let mut es = vec![0, 0, 0]; // ES_ID, flags
    es.extend(descriptor(0x04, &config));
    es.extend(descriptor(0x06, &[0x02])); // SLConfigDescriptor (MP4)
    descriptor(0x03, &es)
}

/// An AudioToolbox input format.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AudioFormat {
    /// `AudioFormatID` (`kAudioFormat*`).
    pub id: u32,
    pub format_flags: u32,
    pub rate: u32,
    pub channels: u32,
    /// 0: variable.
    pub frames_per_packet: u32,
    pub cookie: Option<Vec<u8>>,
}

/// The AudioToolbox format for an audio stream Apple's decoders take, from its setup data and
/// first packet (`None` for the others).
pub(crate) fn audio_format(stream: &StreamInfo, first_packet: &[u8]) -> Option<AudioFormat> {
    let rate = if stream.sample_rate > 0 { stream.sample_rate } else { 48_000 };
    let channels = if stream.channels > 0 { stream.channels as u32 } else { 2 };
    let base = |id, frames_per_packet| AudioFormat { id, format_flags: 0, rate, channels, frames_per_packet, cookie: None };
    match &stream.codec {
        Codec::Ac3 => Some(base(fourcc(b"ac-3"), 1536)),
        Codec::Eac3 => {
            // syncframe: numblkscod (blocks per frame) in byte 4, unless fscod (3) says 6.
            let blocks = match first_packet.get(4) {
                Some(b) if b >> 6 == 3 => 6,
                Some(b) => [1, 2, 3, 6][(b >> 4 & 3) as usize],
                None => 6,
            };
            Some(base(fourcc(b"ec-3"), blocks * 256))
        }
        Codec::Alac => {
            // ALACSpecificConfig: frameLength u32, compatibleVersion, bitDepth, …, numChannels at 9,
            // …, sampleRate u32 at 20.
            let c = stream.extradata.as_deref().filter(|c| c.len() >= 24)?;
            let be32 = |at: usize| u32::from_be_bytes(c[at..at + 4].try_into().unwrap());
            let format_flags = match c[5] {
                16 => 1,
                20 => 2,
                24 => 3,
                32 => 4,
                _ => 0,
            };
            Some(AudioFormat {
                id: fourcc(b"alac"),
                format_flags,
                rate: be32(20).max(1),
                channels: c[9].max(1) as u32,
                frames_per_packet: be32(0),
                cookie: Some(c.to_vec()),
            })
        }
        Codec::Aac => {
            let asc = stream.extradata.as_deref().filter(|a| super::rules::is_usac(a))?;
            Some(AudioFormat { cookie: Some(esds_from_asc(asc)), ..base(fourcc(b"usac"), 0) })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Demuxer, StreamKind};
    use crate::source::FileSource;

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// The first packet of the first stream of `kind`, and that stream.
    fn first(d: &mut dyn Demuxer, kind: StreamKind) -> (StreamInfo, Vec<u8>) {
        let s = d.streams().iter().find(|s| s.kind == kind).unwrap().clone();
        let p = std::iter::from_fn(|| d.next_packet().unwrap()).find(|p| p.stream == s.id).unwrap();
        (s, p.data)
    }

    fn mkv(name: &str) -> Box<dyn Demuxer> {
        Box::new(crate::demux::MatroskaDemuxer::open(Box::new(FileSource::open(fixture(name)).unwrap())).unwrap())
    }

    #[test]
    fn codec_types_and_atoms() {
        use crate::demux::Codec;
        assert_eq!(video_codec_type(&Codec::H264), Some(u32::from_be_bytes(*b"avc1")));
        assert_eq!(video_codec_type(&Codec::Hevc), Some(u32::from_be_bytes(*b"hvc1")));
        assert_eq!(video_codec_type(&Codec::Av1), Some(u32::from_be_bytes(*b"av01")));
        assert_eq!(video_codec_type(&Codec::Vp9), Some(u32::from_be_bytes(*b"vp09")));
        assert_eq!(video_codec_type(&Codec::Vp8), None);
        assert_eq!(atom_name(&Codec::H264), Some("avcC"));
        assert_eq!(atom_name(&Codec::Hevc), Some("hvcC"));
        assert_eq!(atom_name(&Codec::Av1), Some("av1C"));
        assert_eq!(atom_name(&Codec::Vp9), Some("vpcC"));
        assert_eq!(atom_name(&Codec::ProRes), None);
    }

    #[test]
    fn vpcc_from_vp9_keyframes() {
        let (_, frame) = first(mkv("vp9_profile0.webm").as_mut(), StreamKind::Video);
        let v = vpcc_from_keyframe(&frame).unwrap();
        assert_eq!(&v[..4], &[1, 0, 0, 0], "version 1, flags 0");
        assert_eq!(v[4], 0, "profile 0");
        assert_eq!(v[6] >> 4, 8, "8-bit");
        assert_eq!((v[6] >> 1) & 7, 1, "4:2:0");
        assert_eq!(v.len(), 12);
        let (_, frame) = first(mkv("vp9_10bit.webm").as_mut(), StreamKind::Video);
        let v = vpcc_from_keyframe(&frame).unwrap();
        assert_eq!((v[4], v[6] >> 4), (2, 10), "profile 2, 10-bit");
        assert!(vpcc_from_keyframe(&[0x86, 0, 0]).is_none(), "not a keyframe");
        assert!(vpcc_from_keyframe(&[]).is_none());
    }

    #[cfg(feature = "native")]
    #[test]
    fn prores_subtype_from_the_frame_header() {
        let mov = |name: &str| -> Box<dyn Demuxer> {
            Box::new(crate::demux::Mp4Demuxer::open(Box::new(FileSource::open(fixture(name)).unwrap())).unwrap())
        };
        let (_, f) = first(mov("prores_hq.mov").as_mut(), StreamKind::Video);
        assert_eq!(prores_subtype(&f), Some(u32::from_be_bytes(*b"apcn")));
        let (_, f) = first(mov("prores_4444.mov").as_mut(), StreamKind::Video);
        assert_eq!(prores_subtype(&f), Some(u32::from_be_bytes(*b"ap4h")));
        assert_eq!(prores_subtype(b"garbage"), None);
    }

    #[cfg(feature = "native")]
    #[test]
    fn esds_cookie_round_trips() {
        let asc = [0xF9, 0x40, 0x11, 0x22];
        let cookie = esds_from_asc(&asc);
        assert_eq!(cookie[0], 0x03, "ES_Descriptor");
        let boxed = [&[0, 0, 0, 0][..], &cookie].concat();
        let (object_type, specific) = crate::demux::mp4_parse_esds(&boxed).unwrap();
        assert_eq!(object_type, 0x40);
        assert_eq!(specific.as_deref(), Some(&asc[..]));
    }

    #[cfg(feature = "native")]
    #[test]
    fn audio_formats() {
        let (s, p) = first(mkv("ac3.mkv").as_mut(), StreamKind::Audio);
        let f = audio_format(&s, &p).unwrap();
        assert_eq!((f.id, f.rate, f.channels, f.frames_per_packet), (u32::from_be_bytes(*b"ac-3"), s.sample_rate, s.channels as u32, 1536));
        assert!(f.cookie.is_none());
        let (s, p) = first(mkv("eac3.mkv").as_mut(), StreamKind::Audio);
        let f = audio_format(&s, &p).unwrap();
        assert_eq!((f.id, f.frames_per_packet), (u32::from_be_bytes(*b"ec-3"), 1536));
        let mut m = crate::demux::Mp4Demuxer::open(Box::new(FileSource::open(fixture("alac.m4a")).unwrap())).unwrap();
        let (s, p) = first(&mut m, StreamKind::Audio);
        let f = audio_format(&s, &p).unwrap();
        assert_eq!(f.id, u32::from_be_bytes(*b"alac"));
        assert_eq!(f.frames_per_packet, 4096);
        assert!(f.cookie.as_ref().unwrap().len() >= 24);
        assert!(f.format_flags >= 1, "ALAC source bit depth flag");
        let mut aac = StreamInfo::new(1, StreamKind::Audio, crate::demux::Codec::Aac);
        aac.sample_rate = 48_000;
        aac.channels = 2;
        aac.extradata = Some(vec![0x12, 0x10]);
        assert!(audio_format(&aac, &[]).is_none(), "plain AAC stays native");
        aac.extradata = Some(vec![0xF9, 0x40, 0x11, 0x22]);
        assert_eq!(audio_format(&aac, &[]).unwrap().id, u32::from_be_bytes(*b"usac"));
    }
}
