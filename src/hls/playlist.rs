//! M3U8 playlists (RFC 8216): master playlists (variants, audio renditions) and media playlists
//! (segments). Pure parsing: no I/O.

use std::time::Duration;

use url::Url;

use crate::{Error, Result};

/// Longer target or segment durations are treated as malformed (and keep all time arithmetic
/// far from overflow).
const MAX_DURATION: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// The variants of one presentation, and their alternative audio.
#[derive(Clone, Debug, Default)]
pub struct MasterPlaylist {
    pub variants: Vec<VariantStream>,
    /// `EXT-X-MEDIA` renditions with `TYPE=AUDIO` (other types are ignored).
    pub audio: Vec<Rendition>,
}

/// One `EXT-X-STREAM-INF` entry: the same content at one bitrate/resolution.
#[derive(Clone, Debug)]
pub struct VariantStream {
    pub uri: Url,
    /// Peak bits per second.
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    /// RFC 6381 codec strings (`avc1.64001f`, `mp4a.40.2`); empty when not stated.
    pub codecs: Vec<String>,
    pub resolution: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    /// `GROUP-ID` of the audio renditions that go with this variant.
    pub audio_group: Option<String>,
}

/// An alternative audio playlist (`EXT-X-MEDIA:TYPE=AUDIO`).
#[derive(Clone, Debug)]
pub struct Rendition {
    pub group: String,
    pub name: String,
    pub language: Option<String>,
    /// `None`: the audio is the one muxed into the variant's own segments.
    pub uri: Option<Url>,
    pub default: bool,
    pub autoselect: bool,
    pub channels: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistType {
    Vod,
    Event,
}

/// A list of media segments.
#[derive(Clone, Debug)]
pub struct MediaPlaylist {
    pub target_duration: Duration,
    pub media_sequence: u64,
    pub discontinuity_sequence: u64,
    pub segments: Vec<Segment>,
    /// `EXT-X-ENDLIST`: no segments will be added (VOD, or a live stream that ended).
    pub ended: bool,
    pub playlist_type: Option<PlaylistType>,
}

impl MediaPlaylist {
    /// Where segment `index` starts, counting from the playlist's first segment.
    pub fn start_of(&self, index: usize) -> Duration {
        self.segments[..index.min(self.segments.len())].iter().map(|s| s.duration).sum()
    }

    pub fn total_duration(&self) -> Duration {
        self.start_of(self.segments.len())
    }

    /// The segment holding time `t` (counted like `start_of`); the last one past the end.
    pub fn index_at(&self, t: Duration) -> Option<usize> {
        let mut start = Duration::ZERO;
        for (i, s) in self.segments.iter().enumerate() {
            if t < start + s.duration {
                return Some(i);
            }
            start += s.duration;
        }
        self.segments.len().checked_sub(1)
    }
}

#[derive(Clone, Debug)]
pub struct Segment {
    pub uri: Url,
    pub duration: Duration,
    /// Media sequence number.
    pub sequence: u64,
    pub discontinuity_seq: u64,
    /// `(offset, length)` within the resource.
    pub byte_range: Option<(u64, u64)>,
    pub key: Option<Key>,
    /// Initialization section (fragmented MP4).
    pub map: Option<InitSection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyMethod {
    Aes128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub method: KeyMethod,
    pub uri: Url,
    /// `None`: the IV is the segment's media sequence number.
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InitSection {
    pub uri: Url,
    pub byte_range: Option<(u64, u64)>,
}

/// Parses a playlist; relative URIs are resolved against `base` (the playlist's own URL, after
/// redirects).
pub fn parse(text: &str, base: &Url) -> Result<Playlist> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
    if lines.next().map(|(_, l)| l) != Some("#EXTM3U") {
        return Err(Error::InvalidSource(format!("{base}: not an M3U8 playlist")));
    }
    let err = |n: usize, what: &str| Error::InvalidSource(format!("{base}: line {n}: {what}"));
    let resolve = |n: usize, uri: &str| base.join(uri).map_err(|e| err(n, &format!("bad URI {uri:?}: {e}")));

    let mut master = MasterPlaylist::default();
    let mut is_master = false;
    let mut pending_variant: Option<(usize, Vec<(String, String)>)> = None;

    let mut media = MediaPlaylist {
        target_duration: Duration::ZERO,
        media_sequence: 0,
        discontinuity_sequence: 0,
        segments: Vec::new(),
        ended: false,
        playlist_type: None,
    };
    let mut duration: Option<Duration> = None;
    let mut range: Option<(Option<u64>, u64)> = None;
    let mut discontinuity = 0u64;
    let mut key: Option<Key> = None;
    let mut map: Option<InitSection> = None;
    // Where the previous byte range of each URI ended, for ranges without an offset.
    let mut range_end: Option<(Url, u64)> = None;

    for (n, line) in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(tag) = line.strip_prefix('#') {
            let (name, value) = tag.split_once(':').unwrap_or((tag, ""));
            match name {
                "EXT-X-STREAM-INF" => {
                    is_master = true;
                    pending_variant = Some((n, attributes(value)));
                }
                "EXT-X-MEDIA" => {
                    let a = attributes(value);
                    if attr(&a, "TYPE") != Some("AUDIO") {
                        continue;
                    }
                    master.audio.push(Rendition {
                        group: attr(&a, "GROUP-ID").ok_or_else(|| err(n, "EXT-X-MEDIA without GROUP-ID"))?.to_owned(),
                        name: attr(&a, "NAME").unwrap_or_default().to_owned(),
                        language: attr(&a, "LANGUAGE").map(str::to_owned),
                        uri: attr(&a, "URI").map(|u| resolve(n, u)).transpose()?,
                        default: attr(&a, "DEFAULT") == Some("YES"),
                        autoselect: attr(&a, "AUTOSELECT") == Some("YES"),
                        channels: attr(&a, "CHANNELS").map(str::to_owned),
                    });
                }
                "EXT-X-TARGETDURATION" => {
                    let secs: u64 = value.trim().parse().map_err(|_| err(n, "bad EXT-X-TARGETDURATION"))?;
                    media.target_duration = Some(Duration::from_secs(secs.min(u64::MAX / 2)))
                        .filter(|d| *d <= MAX_DURATION)
                        .ok_or_else(|| err(n, "EXT-X-TARGETDURATION too large"))?;
                }
                "EXT-X-MEDIA-SEQUENCE" => {
                    media.media_sequence = value.trim().parse().map_err(|_| err(n, "bad EXT-X-MEDIA-SEQUENCE"))?;
                }
                "EXT-X-DISCONTINUITY-SEQUENCE" => {
                    media.discontinuity_sequence =
                        value.trim().parse().map_err(|_| err(n, "bad EXT-X-DISCONTINUITY-SEQUENCE"))?;
                }
                "EXT-X-PLAYLIST-TYPE" => {
                    media.playlist_type = match value.trim() {
                        "VOD" => Some(PlaylistType::Vod),
                        "EVENT" => Some(PlaylistType::Event),
                        _ => None,
                    };
                }
                "EXT-X-ENDLIST" => media.ended = true,
                "EXTINF" => {
                    let secs = value.split(',').next().unwrap_or("").trim();
                    let secs: f64 = secs.parse().map_err(|_| err(n, "bad EXTINF duration"))?;
                    let d = Duration::try_from_secs_f64(secs).ok().filter(|d| *d <= MAX_DURATION);
                    duration = Some(d.ok_or_else(|| err(n, "bad EXTINF duration"))?);
                }
                "EXT-X-BYTERANGE" => range = Some(parse_range(value).ok_or_else(|| err(n, "bad EXT-X-BYTERANGE"))?),
                "EXT-X-DISCONTINUITY" => discontinuity += 1,
                "EXT-X-KEY" => {
                    let a = attributes(value);
                    key = match attr(&a, "METHOD") {
                        Some("NONE") => None,
                        Some("AES-128") => {
                            if attr(&a, "KEYFORMAT").is_some_and(|f| f != "identity") {
                                return Err(Error::Unsupported("DRM-protected HLS"));
                            }
                            let uri = attr(&a, "URI").ok_or_else(|| err(n, "EXT-X-KEY without URI"))?;
                            let iv = attr(&a, "IV").map(|v| parse_iv(v).ok_or_else(|| err(n, "bad IV"))).transpose()?;
                            Some(Key { method: KeyMethod::Aes128, uri: resolve(n, uri)?, iv })
                        }
                        _ => return Err(Error::Unsupported("DRM-protected HLS")),
                    };
                }
                "EXT-X-MAP" => {
                    let a = attributes(value);
                    let uri = attr(&a, "URI").ok_or_else(|| err(n, "EXT-X-MAP without URI"))?;
                    let byte_range = match attr(&a, "BYTERANGE") {
                        Some(r) => match parse_range(r) {
                            Some((offset, len)) => Some((offset.unwrap_or(0), len)),
                            None => return Err(err(n, "bad EXT-X-MAP BYTERANGE")),
                        },
                        None => None,
                    };
                    map = Some(InitSection { uri: resolve(n, uri)?, byte_range });
                }
                _ => {}
            }
            continue;
        }
        // A URI line.
        let uri = resolve(n, line)?;
        if let Some((vn, a)) = pending_variant.take() {
            let bandwidth = attr(&a, "BANDWIDTH")
                .and_then(|b| b.parse().ok())
                .ok_or_else(|| err(vn, "EXT-X-STREAM-INF without BANDWIDTH"))?;
            master.variants.push(VariantStream {
                uri,
                bandwidth,
                average_bandwidth: attr(&a, "AVERAGE-BANDWIDTH").and_then(|b| b.parse().ok()),
                codecs: attr(&a, "CODECS")
                    .map(|c| c.split(',').map(|c| c.trim().to_owned()).filter(|c| !c.is_empty()).collect())
                    .unwrap_or_default(),
                resolution: attr(&a, "RESOLUTION").and_then(|r| {
                    let (w, h) = r.split_once(['x', 'X'])?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                }),
                frame_rate: attr(&a, "FRAME-RATE").and_then(|f| f.parse().ok()),
                audio_group: attr(&a, "AUDIO").map(str::to_owned),
            });
            continue;
        }
        if is_master {
            continue;
        }
        let byte_range = match range.take() {
            Some((offset, len)) => {
                let offset = match offset {
                    Some(o) => o,
                    None => match &range_end {
                        Some((u, end)) if *u == uri => *end,
                        _ => return Err(err(n, "EXT-X-BYTERANGE without offset does not follow a range of the same URI")),
                    },
                };
                range_end = Some((uri.clone(), offset + len));
                Some((offset, len))
            }
            None => None,
        };
        media.segments.push(Segment {
            uri,
            duration: duration.take().ok_or_else(|| err(n, "segment without EXTINF"))?,
            sequence: media.media_sequence + media.segments.len() as u64,
            discontinuity_seq: media.discontinuity_sequence + discontinuity,
            byte_range,
            key: key.clone(),
            map: map.clone(),
        });
    }
    if is_master {
        if master.variants.is_empty() {
            return Err(Error::InvalidSource(format!("{base}: master playlist without variants")));
        }
        return Ok(Playlist::Master(master));
    }
    Ok(Playlist::Media(media))
}

/// An attribute list (`A=1,B="x,y",C=0x10`), quotes removed.
fn attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        out.push((name.trim().to_owned(), value.to_owned()));
        rest = next.trim_start_matches(',').trim_start();
    }
    out
}

fn attr<'a>(a: &'a [(String, String)], name: &str) -> Option<&'a str> {
    a.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// `len[@offset]`.
fn parse_range(s: &str) -> Option<(Option<u64>, u64)> {
    let s = s.trim();
    match s.split_once('@') {
        Some((len, offset)) => Some((Some(offset.parse().ok()?), len.parse().ok()?)),
        None => Some((None, s.parse().ok()?)),
    }
}

/// `0x` + up to 32 hex digits, big-endian, left-padded.
fn parse_iv(s: &str) -> Option<[u8; 16]> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 32 {
        return None;
    }
    let value = u128::from_str_radix(hex, 16).ok()?;
    Some(value.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/path/master.m3u8?token=abc").unwrap()
    }

    #[test]
    fn master_with_variants_and_audio_group() {
        let text = "\u{feff}#EXTM3U\r\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"2\",URI=\"audio/en.m3u8\"\r\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"x\",URI=\"s.m3u8\"\r\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,AVERAGE-BANDWIDTH=1000000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=640x360,FRAME-RATE=29.970,AUDIO=\"aud\"\r\nlow/index.m3u8\r\n#EXT-X-STREAM-INF:BANDWIDTH=2560000,RESOLUTION=1280x720\r\nhttps://other.example.net/hi.m3u8\r\n";
        let Playlist::Master(m) = parse(text, &base()).unwrap() else { panic!("not a master playlist") };
        assert_eq!(m.variants.len(), 2);
        let v = &m.variants[0];
        assert_eq!(v.uri.as_str(), "https://cdn.example.com/path/low/index.m3u8");
        assert_eq!((v.bandwidth, v.average_bandwidth, v.resolution), (1_280_000, Some(1_000_000), Some((640, 360))));
        assert_eq!(v.codecs, ["avc1.4d401f", "mp4a.40.2"], "a quoted comma does not split the attribute list");
        assert_eq!(v.audio_group.as_deref(), Some("aud"));
        assert!((v.frame_rate.unwrap() - 29.97).abs() < 1e-6);
        assert_eq!(m.variants[1].uri.as_str(), "https://other.example.net/hi.m3u8");
        assert!(m.variants[1].codecs.is_empty());
        assert_eq!(m.audio.len(), 1, "subtitle renditions are ignored");
        let a = &m.audio[0];
        assert_eq!((a.group.as_str(), a.name.as_str(), a.language.as_deref(), a.default), ("aud", "English", Some("en"), true));
        assert_eq!(a.uri.as_ref().unwrap().as_str(), "https://cdn.example.com/path/audio/en.m3u8");
    }

    #[test]
    fn media_playlist_with_keys_ranges_map_and_discontinuity() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.000,\n#EXT-X-BYTERANGE:1000@720\nmain.mp4\n#EXTINF:3.5,\n#EXT-X-BYTERANGE:500\nmain.mp4\n#EXT-X-DISCONTINUITY\n#EXT-X-KEY:METHOD=AES-128,URI=\"key2.bin\",IV=0x000102030405060708090a0b0c0d0e0f\n#EXTINF:2,\nseg3.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:2,\nseg4.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(p) = parse(text, &base()).unwrap() else { panic!("not a media playlist") };
        assert_eq!((p.target_duration, p.media_sequence, p.ended), (Duration::from_secs(4), 100, true));
        assert_eq!(p.playlist_type, Some(PlaylistType::Vod));
        assert_eq!(p.segments.len(), 4);
        let s = &p.segments;
        assert_eq!((s[0].sequence, s[0].byte_range), (100, Some((720, 1000))));
        assert_eq!(s[1].byte_range, Some((1720, 500)), "a range without @ follows the previous one");
        assert_eq!(s[0].map.as_ref().unwrap().byte_range, Some((0, 720)));
        assert_eq!(s[0].map.as_ref().unwrap().uri.as_str(), "https://cdn.example.com/path/init.mp4");
        assert_eq!(s[0].key.as_ref().unwrap().iv, None);
        assert_eq!(s[0].key.as_ref().unwrap().uri.as_str(), "https://cdn.example.com/path/key.bin");
        assert_eq!((s[1].discontinuity_seq, s[2].discontinuity_seq), (0, 1));
        assert_eq!(s[2].key.as_ref().unwrap().iv, Some(core::array::from_fn(|i| i as u8)));
        assert!(s[3].key.is_none(), "METHOD=NONE clears the key");
        assert!(s[3].map.is_some(), "the map applies until replaced");
        assert_eq!(p.start_of(2), Duration::from_millis(7500));
        assert_eq!(p.total_duration(), Duration::from_millis(11_500));
        assert_eq!(s[0].duration, Duration::from_secs(4));
    }

    #[test]
    fn live_playlist_has_no_endlist() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts?x=1\n";
        let Playlist::Media(p) = parse(text, &base()).unwrap() else { panic!("not a media playlist") };
        assert!(!p.ended);
        assert_eq!(p.segments.iter().map(|s| s.sequence).collect::<Vec<_>>(), [7, 8]);
        assert_eq!(p.segments[0].discontinuity_seq, 3);
        assert_eq!(p.segments[1].uri.as_str(), "https://cdn.example.com/path/b.ts?x=1");
    }

    #[test]
    fn drm_and_errors() {
        let drm = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:2,\na.ts\n";
        assert!(matches!(parse(drm, &base()), Err(Error::Unsupported("DRM-protected HLS"))));
        let widevine = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",KEYFORMAT=\"urn:uuid:edef8ba9\"\n#EXTINF:2,\na.ts\n";
        assert!(matches!(parse(widevine, &base()), Err(Error::Unsupported(_))));
        assert!(matches!(parse("hello", &base()), Err(Error::InvalidSource(_))));
        let bad = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:abc,\na.ts\n";
        match parse(bad, &base()) {
            Err(Error::InvalidSource(m)) => assert!(m.contains("line 3"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn absurd_durations_are_errors_not_panics() {
        for text in [
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1e300,\na.ts\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:18446744073709551615\n#EXTINF:2,\na.ts\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:100000,\na.ts\n",
        ] {
            assert!(matches!(parse(text, &base()), Err(Error::InvalidSource(_))), "{text:?}");
        }
    }
}
