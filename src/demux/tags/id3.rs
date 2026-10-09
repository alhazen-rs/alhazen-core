//! ID3v2 (2.2/2.3/2.4) and ID3v1 tags.

/// Total length of the ID3v2 tag whose 10-byte header starts `head` (header, body and footer),
/// or `None` when `head` is not an ID3v2 header.
pub(crate) fn id3v2_len(head: &[u8]) -> Option<u64> {
    if head.len() < 10 || &head[..3] != b"ID3" || head[3] == 0xFF || head[4] == 0xFF {
        return None;
    }
    let size = synchsafe(&head[6..10])?;
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    Some(10 + size as u64 + footer)
}

/// A 28-bit "synchsafe" integer (7 bits per byte).
pub(crate) fn synchsafe(b: &[u8]) -> Option<u32> {
    if b.len() != 4 || b.iter().any(|x| x & 0x80 != 0) {
        return None;
    }
    Some(b.iter().fold(0, |v, &x| v << 7 | x as u32))
}

#[cfg(feature = "native")]
use std::borrow::Cow;

#[cfg(feature = "native")]
use crate::demux::metadata::{CoverPick, Field, Metadata};

/// Title, artist, album, album artist, track, year, genre and pictures of a whole ID3v2.2/2.3/2.4
/// tag (`tag` starts with its 10-byte header). Malformed frames end the walk; fields read so far
/// stay.
#[cfg(feature = "native")]
pub(crate) fn parse_id3v2(tag: &[u8], meta: &mut Metadata) {
    let Some(len) = id3v2_len(tag) else { return };
    let (version, flags) = (tag[3], tag[5]);
    if !(2..=4).contains(&version) {
        return;
    }
    let footer = if flags & 0x10 != 0 { 10 } else { 0 };
    let end = (len as usize).saturating_sub(footer).min(tag.len());
    let raw = tag.get(10..end).unwrap_or(&[]);
    // v2.2/v2.3 unsynchronise the whole tag; v2.4 does it per frame.
    let body: Cow<[u8]> = if flags & 0x80 != 0 && version < 4 { Cow::Owned(unsync(raw)) } else { Cow::Borrowed(raw) };
    let mut pos = 0usize;
    if flags & 0x40 != 0 && version >= 3 {
        let Some(size) = body.get(..4) else { return };
        pos = if version == 4 {
            synchsafe(size).unwrap_or(u32::MAX) as usize
        } else {
            u32::from_be_bytes([size[0], size[1], size[2], size[3]]) as usize + 4
        };
    }
    let (id_len, head_len) = if version == 2 { (3, 6) } else { (4, 10) };
    let mut covers = CoverPick::default();
    while let Some(head) = body.get(pos..pos + head_len) {
        if head[0] == 0 {
            break; // padding
        }
        let size = match version {
            2 => u32::from_be_bytes([0, head[3], head[4], head[5]]),
            3 => u32::from_be_bytes([head[4], head[5], head[6], head[7]]),
            _ => synchsafe(&head[4..8]).unwrap_or(u32::MAX),
        } as usize;
        let frame_flags = if version == 2 { 0 } else { u16::from_be_bytes([head[8], head[9]]) };
        let start = pos + head_len;
        let Some(data) = body.get(start..start.saturating_add(size)) else { break };
        pos = start + size;
        if let Some(data) = frame_data(version, frame_flags, data) {
            apply_frame(&head[..id_len], &data, meta, &mut covers);
        }
    }
    covers.finish(meta);
}

/// A frame's content with the v2.3/v2.4 frame flags applied; `None` for compressed or encrypted
/// frames.
#[cfg(feature = "native")]
fn frame_data(version: u8, flags: u16, data: &[u8]) -> Option<Cow<'_, [u8]>> {
    let (unreadable, grouping) = match version {
        3 => (flags & 0x00C0 != 0, flags & 0x0020 != 0),
        4 => (flags & 0x000C != 0, flags & 0x0040 != 0),
        _ => (false, false),
    };
    if unreadable {
        return None;
    }
    let mut data = data;
    if grouping {
        data = data.get(1..)?;
    }
    if version == 4 && flags & 0x0001 != 0 {
        data = data.get(4..)?; // data length indicator
    }
    Some(if version == 4 && flags & 0x0002 != 0 { Cow::Owned(unsync(data)) } else { Cow::Borrowed(data) })
}

#[cfg(feature = "native")]
fn apply_frame(id: &[u8], data: &[u8], meta: &mut Metadata, covers: &mut CoverPick) {
    let field = match id {
        b"TIT2" | b"TT2" => Field::Title,
        b"TPE1" | b"TP1" => Field::Artist,
        b"TALB" | b"TAL" => Field::Album,
        b"TPE2" | b"TP2" => Field::AlbumArtist,
        b"TRCK" | b"TRK" => Field::Track,
        b"TDRC" | b"TYER" | b"TYE" => Field::Year,
        b"TCON" | b"TCO" => Field::Genre,
        b"APIC" => return picture(data, false, covers),
        b"PIC" => return picture(data, true, covers),
        _ => return,
    };
    let value = text(data);
    meta.set(field, &if field == Field::Genre { genre(&value) } else { value });
}

/// A text frame: an encoding byte, then text. v2.4 may hold several NUL-separated values; the first
/// is used.
#[cfg(feature = "native")]
fn text(data: &[u8]) -> String {
    let Some((&encoding, rest)) = data.split_first() else { return String::new() };
    decode(encoding, rest).split('\0').next().unwrap_or("").to_owned()
}

#[cfg(feature = "native")]
fn decode(encoding: u8, b: &[u8]) -> String {
    match encoding {
        0 => b.iter().map(|&c| c as char).collect(),
        1 => utf16(b, None),
        2 => utf16(b, Some(false)),
        3 => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    }
}

/// UTF-16 with a byte-order mark (`little_endian: None`) or of a fixed byte order.
#[cfg(feature = "native")]
fn utf16(b: &[u8], little_endian: Option<bool>) -> String {
    let (le, b) = match (little_endian, b) {
        (Some(le), b) => (le, b),
        (None, [0xFF, 0xFE, rest @ ..]) => (true, rest),
        (None, [0xFE, 0xFF, rest @ ..]) => (false, rest),
        (None, b) => (true, b),
    };
    let units = b.as_chunks::<2>().0.iter().map(|&c| if le { u16::from_le_bytes(c) } else { u16::from_be_bytes(c) });
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

/// `APIC` (v2.3/2.4: MIME string) or `PIC` (v2.2: 3-letter format): encoding, format, picture
/// type, description, data.
#[cfg(feature = "native")]
fn picture(d: &[u8], v22: bool, covers: &mut CoverPick) {
    let Some((&encoding, rest)) = d.split_first() else { return };
    let (mime, rest) = if v22 {
        let Some(format) = rest.get(..3) else { return };
        let mime = match &format.to_ascii_uppercase()[..] {
            b"PNG" => "image/png",
            b"JPG" => "image/jpeg",
            _ => "",
        };
        (mime.to_owned(), &rest[3..])
    } else {
        let Some(nul) = rest.iter().position(|&b| b == 0) else { return };
        (String::from_utf8_lossy(&rest[..nul]).to_ascii_lowercase(), &rest[nul + 1..])
    };
    if mime == "-->" {
        return; // a link, not a picture
    }
    let Some((&kind, rest)) = rest.split_first() else { return };
    let data = if encoding == 1 || encoding == 2 {
        rest.as_chunks::<2>().0.iter().position(|c| c == &[0, 0]).and_then(|i| rest.get(i * 2 + 2..))
    } else {
        rest.iter().position(|&b| b == 0).and_then(|i| rest.get(i + 1..))
    };
    if let Some(data) = data {
        covers.offer(kind == 3, &mime, data);
    }
}

/// `(17)`, `17` and `(17)Rock` style genres; text is kept as is.
#[cfg(feature = "native")]
fn genre(s: &str) -> String {
    let t = s.trim();
    let name = |n: &str| n.parse::<usize>().ok().and_then(|i| GENRES.get(i)).map(|g| g.to_string());
    if let Some(g) = name(t) {
        return g;
    }
    if let Some((n, rest)) = t.strip_prefix('(').and_then(|r| r.split_once(')')) {
        if !rest.trim().is_empty() {
            return rest.trim().to_owned();
        }
        if let Some(g) = name(n) {
            return g;
        }
    }
    t.to_owned()
}

/// Removes ID3 unsynchronisation (`FF 00` → `FF`).
#[cfg(feature = "native")]
fn unsync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut prev = 0u8;
    for &x in b {
        if !(prev == 0xFF && x == 0) {
            out.push(x);
        }
        prev = x;
    }
    out
}

/// An ID3v1/v1.1 trailer (the last 128 bytes of the file). Fills only fields still empty.
#[cfg(feature = "native")]
pub(crate) fn parse_id3v1(t: &[u8], meta: &mut Metadata) {
    if t.len() != 128 || &t[..3] != b"TAG" {
        return;
    }
    let s = |b: &[u8]| -> String { b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect() };
    meta.set(Field::Title, &s(&t[3..33]));
    meta.set(Field::Artist, &s(&t[33..63]));
    meta.set(Field::Album, &s(&t[63..93]));
    meta.set(Field::Year, &s(&t[93..97]));
    if t[125] == 0 && t[126] != 0 {
        meta.set(Field::Track, &t[126].to_string());
    }
    if let Some(g) = GENRES.get(t[127] as usize) {
        meta.set(Field::Genre, g);
    }
}

/// ID3v1 genres 0–79.
#[cfg(feature = "native")]
pub(crate) const GENRES: [&str; 80] = [
    "Blues", "Classic Rock", "Country", "Dance", "Disco", "Funk", "Grunge", "Hip-Hop", "Jazz", "Metal",
    "New Age", "Oldies", "Other", "Pop", "R&B", "Rap", "Reggae", "Rock", "Techno", "Industrial",
    "Alternative", "Ska", "Death Metal", "Pranks", "Soundtrack", "Euro-Techno", "Ambient", "Trip-Hop", "Vocal", "Jazz+Funk",
    "Fusion", "Trance", "Classical", "Instrumental", "Acid", "House", "Game", "Sound Clip", "Gospel", "Noise",
    "AlternRock", "Bass", "Soul", "Punk", "Space", "Meditative", "Instrumental Pop", "Instrumental Rock", "Ethnic", "Gothic",
    "Darkwave", "Techno-Industrial", "Electronic", "Pop-Folk", "Eurodance", "Dream", "Southern Rock", "Comedy", "Cult", "Gangsta",
    "Top 40", "Christian Rap", "Pop/Funk", "Jungle", "Native American", "Cabaret", "New Wave", "Psychedelic", "Rave", "Showtunes",
    "Trailer", "Lo-Fi", "Tribal", "Acid Punk", "Acid Jazz", "Polka", "Retro", "Musical", "Rock & Roll", "Hard Rock",
];

#[cfg(test)]
mod length_tests {
    use super::*;

    #[test]
    fn tag_length_includes_header_and_footer() {
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x02\x01"), Some(10 + 257));
        assert_eq!(id3v2_len(b"ID3\x04\x00\x10\x00\x00\x00\x05"), Some(10 + 5 + 10));
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x80\x00"), None, "not synchsafe");
        assert_eq!(id3v2_len(b"TAG"), None);
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::demux::metadata::Metadata;

    fn synchsafe_bytes(n: u32) -> [u8; 4] {
        [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]
    }

    fn frame(version: u8, id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        match version {
            2 => f.extend(&(body.len() as u32).to_be_bytes()[1..]),
            3 => f.extend((body.len() as u32).to_be_bytes().into_iter().chain([0, 0])),
            _ => f.extend(synchsafe_bytes(body.len() as u32).into_iter().chain([0, 0])),
        }
        f.extend(body);
        f
    }

    fn tag(version: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let mut t = vec![b'I', b'D', b'3', version, 0, flags];
        t.extend(synchsafe_bytes(body.len() as u32));
        t.extend(body);
        t
    }

    fn latin1(s: &str) -> Vec<u8> {
        std::iter::once(0).chain(s.chars().map(|c| c as u8)).collect()
    }

    fn parsed(t: &[u8]) -> Metadata {
        let mut m = Metadata::default();
        parse_id3v2(t, &mut m);
        m
    }

    #[test]
    fn v23_text_frames_and_numeric_genre() {
        let body = [
            frame(3, b"TIT2", &latin1("Song")),
            frame(3, b"TPE1", &latin1("Band")),
            frame(3, b"TALB", &latin1("Record")),
            frame(3, b"TPE2", &latin1("Various")),
            frame(3, b"TRCK", &latin1("3/12")),
            frame(3, b"TYER", &latin1("2024")),
            frame(3, b"TCON", &latin1("(17)")),
        ]
        .concat();
        let m = parsed(&tag(3, 0, &body));
        assert_eq!(m.title.as_deref(), Some("Song"));
        assert_eq!((m.artist.as_deref(), m.album.as_deref(), m.album_artist.as_deref()), (Some("Band"), Some("Record"), Some("Various")));
        assert_eq!((m.track, m.year, m.genre.as_deref()), (Some(3), Some(2024), Some("Rock")));
    }

    #[test]
    fn v24_utf8_and_utf16_text() {
        let mut utf16 = vec![1, 0xFF, 0xFE]; // UTF-16 with a little-endian BOM
        utf16.extend("Ünïcode".encode_utf16().flat_map(u16::to_le_bytes));
        let mut utf16be = vec![2];
        utf16be.extend("Beta".encode_utf16().flat_map(u16::to_be_bytes));
        let body = [
            frame(4, b"TIT2", &[&[3u8][..], "Café".as_bytes()].concat()),
            frame(4, b"TPE1", &utf16),
            frame(4, b"TALB", &utf16be),
            frame(4, b"TDRC", &latin1("2023-04-01")),
        ]
        .concat();
        let m = parsed(&tag(4, 0, &body));
        assert_eq!((m.title.as_deref(), m.artist.as_deref(), m.album.as_deref()), (Some("Café"), Some("Ünïcode"), Some("Beta")));
        assert_eq!(m.year, Some(2023));
    }

    #[test]
    fn v22_frames_and_pic() {
        let mut pic = vec![0];
        pic.extend(b"PNG");
        pic.push(3); // front cover
        pic.push(0); // empty description
        pic.extend(b"\x89PNG data");
        let body = [frame(2, b"TT2", &latin1("Old")), frame(2, b"PIC", &pic)].concat();
        let m = parsed(&tag(2, 0, &body));
        assert_eq!(m.title.as_deref(), Some("Old"));
        let cover = m.cover.unwrap();
        assert_eq!((cover.mime.as_str(), &cover.data[..]), ("image/png", &b"\x89PNG data"[..]));
    }

    #[test]
    fn the_front_cover_wins_and_huge_pictures_are_skipped() {
        let apic = |kind: u8, mime: &str, data: &[u8]| {
            let mut a = vec![0];
            a.extend(mime.as_bytes());
            a.extend([0, kind, 0]);
            a.extend(data);
            a
        };
        let huge = vec![0xFFu8; crate::demux::metadata::MAX_PICTURE + 1];
        let body = [
            frame(3, b"APIC", &apic(3, "image/jpeg", &huge)),
            frame(3, b"APIC", &apic(0, "image/png", b"\x89PNG other")),
            frame(3, b"APIC", &apic(3, "image/jpeg", &[0xFF, 0xD8, 7])),
        ]
        .concat();
        let cover = parsed(&tag(3, 0, &body)).cover.unwrap();
        assert_eq!((cover.mime.as_str(), &cover.data[..]), ("image/jpeg", &[0xFF, 0xD8, 7][..]));
    }

    #[test]
    fn whole_tag_unsynchronisation_v23() {
        let plain = frame(3, b"TIT2", &[0, b'A', 0xFF, b'B']);
        let mut synced = Vec::new();
        for &b in &plain {
            synced.push(b);
            if b == 0xFF {
                synced.push(0);
            }
        }
        assert_eq!(parsed(&tag(3, 0x80, &synced)).title.as_deref(), Some("AÿB"));
    }

    #[test]
    fn a_truncated_frame_keeps_what_came_before() {
        let mut body = frame(3, b"TIT2", &latin1("Kept"));
        body.extend(b"TPE1\x00\x00\x03\xE8\x00\x00abc"); // claims 1000 bytes, has 3
        assert_eq!(parsed(&tag(3, 0, &body)).title.as_deref(), Some("Kept"));
        assert_eq!(parsed(&tag(3, 0, &body)).artist, None);
        assert!(parsed(b"ID3\x03\x00\x00\x00\x00\x00\x09garbage").is_empty());
    }

    #[test]
    fn id3v1_fills_only_missing_fields() {
        let mut t = vec![0u8; 128];
        t[..3].copy_from_slice(b"TAG");
        t[3..8].copy_from_slice(b"Title");
        t[33..39].copy_from_slice(b"Artist");
        t[93..97].copy_from_slice(b"1999");
        t[126] = 7; // ID3v1.1 track
        t[127] = 8; // Jazz
        let mut m = Metadata::default();
        m.set(crate::demux::metadata::Field::Title, "From v2");
        parse_id3v1(&t, &mut m);
        assert_eq!((m.title.as_deref(), m.artist.as_deref()), (Some("From v2"), Some("Artist")));
        assert_eq!((m.year, m.track, m.genre.as_deref()), (Some(1999), Some(7), Some("Jazz")));
    }

    #[test]
    fn reads_the_tagged_fixture() {
        let bytes = std::fs::read("tests/fixtures/mp3_tagged.mp3").unwrap();
        let len = id3v2_len(&bytes).unwrap() as usize;
        let m = parsed(&bytes[..len]);
        assert_eq!((m.title.as_deref(), m.artist.as_deref(), m.album.as_deref()), (Some("Test Title"), Some("Test Artist"), Some("Test Album")));
        assert_eq!((m.album_artist.as_deref(), m.track, m.year, m.genre.as_deref()), (Some("Test Album Artist"), Some(3), Some(2024), Some("Rock")));
        let cover = m.cover.expect("APIC");
        assert_eq!(cover.mime, "image/png");
        assert_eq!(&cover.data[..], &std::fs::read("tests/fixtures/cover.png").unwrap()[..]);
    }
}
