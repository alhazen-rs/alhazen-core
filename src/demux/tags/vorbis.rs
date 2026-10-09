//! Vorbis comments (FLAC, Ogg Vorbis/Opus/FLAC) and FLAC `PICTURE` blocks, including base64
//! `METADATA_BLOCK_PICTURE`.

use super::Bytes;
use crate::demux::metadata::{CoverPick, Field, MAX_PICTURE, Metadata};

/// A Vorbis comment block starting at its vendor length (callers strip `\x03vorbis` / `OpusTags`).
pub(crate) fn parse_vorbis_comment(body: &[u8], meta: &mut Metadata, covers: &mut CoverPick) {
    let mut r = Bytes::le(body);
    let Some(vendor) = r.u32() else { return };
    if r.skip(vendor as usize).is_none() {
        return;
    }
    let Some(count) = r.u32() else { return };
    for _ in 0..count {
        let Some(len) = r.u32() else { return };
        let Some(entry) = r.take(len as usize) else { return };
        let Some(eq) = entry.iter().position(|&b| b == b'=') else { continue };
        let key = String::from_utf8_lossy(&entry[..eq]).to_ascii_uppercase();
        let value = &entry[eq + 1..];
        let field = match key.as_str() {
            "TITLE" => Field::Title,
            "ARTIST" => Field::Artist,
            "ALBUM" => Field::Album,
            "ALBUMARTIST" | "ALBUM ARTIST" | "ALBUM_ARTIST" => Field::AlbumArtist,
            "TRACKNUMBER" => Field::Track,
            "DATE" | "YEAR" => Field::Year,
            "GENRE" => Field::Genre,
            "METADATA_BLOCK_PICTURE" if value.len() <= MAX_PICTURE / 3 * 4 + 1024 => {
                if let Some(raw) = std::str::from_utf8(value).ok().and_then(base64)
                    && let Some((front, mime, data)) = parse_flac_picture(&raw)
                {
                    covers.offer(front, &mime, data);
                }
                continue;
            }
            _ => continue,
        };
        meta.set(field, &String::from_utf8_lossy(value));
    }
}

/// A FLAC `PICTURE` block body: (front cover, MIME type, image data).
pub(crate) fn parse_flac_picture(b: &[u8]) -> Option<(bool, String, &[u8])> {
    let mut r = Bytes::be(b);
    let kind = r.u32()?;
    let mime_len = r.u32()? as usize;
    let mime = String::from_utf8_lossy(r.take(mime_len)?).to_ascii_lowercase();
    let description = r.u32()? as usize;
    r.skip(description)?;
    r.skip(16)?; // width, height, colour depth, palette size
    let len = r.u32()? as usize;
    let data = r.take(len)?;
    let mime = if mime == "image/jpg" { "image/jpeg".to_owned() } else { mime };
    Some((kind == 3, mime, data))
}

/// Standard base64 (padding optional, whitespace ignored).
pub(crate) fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::metadata::{CoverPick, Metadata};

    fn comment_block(entries: &[&str]) -> Vec<u8> {
        let mut b = 1u32.to_le_bytes().to_vec();
        b.push(b'x'); // vendor
        b.extend((entries.len() as u32).to_le_bytes());
        for e in entries {
            b.extend((e.len() as u32).to_le_bytes());
            b.extend(e.as_bytes());
        }
        b
    }

    fn picture_block(kind: u32, mime: &str, data: &[u8]) -> Vec<u8> {
        let mut b = kind.to_be_bytes().to_vec();
        b.extend((mime.len() as u32).to_be_bytes());
        b.extend(mime.as_bytes());
        b.extend(0u32.to_be_bytes()); // description
        b.extend([0u8; 16]); // width, height, depth, colours
        b.extend((data.len() as u32).to_be_bytes());
        b.extend(data);
        b
    }

    fn encode64(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in data.chunks(3) {
            let n = c.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
            for i in 0..4 {
                s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    }

    fn parsed(body: &[u8]) -> Metadata {
        let (mut m, mut covers) = (Metadata::default(), CoverPick::default());
        parse_vorbis_comment(body, &mut m, &mut covers);
        covers.finish(&mut m);
        m
    }

    #[test]
    fn keys_are_case_insensitive() {
        let m = parsed(&comment_block(&["title=Song", "ARTIST=Band", "Album=Record", "ALBUMARTIST=Various", "TRACKNUMBER=3", "DATE=2024-01-01", "GENRE=Jazz", "NOEQUALS"]));
        assert_eq!((m.title.as_deref(), m.artist.as_deref(), m.album.as_deref()), (Some("Song"), Some("Band"), Some("Record")));
        assert_eq!((m.album_artist.as_deref(), m.track, m.year, m.genre.as_deref()), (Some("Various"), Some(3), Some(2024), Some("Jazz")));
    }

    #[test]
    fn metadata_block_picture_is_decoded() {
        let entry = format!("METADATA_BLOCK_PICTURE={}", encode64(&picture_block(3, "image/png", b"\x89PNG pic")));
        let cover = parsed(&comment_block(&["TITLE=x", &entry])).cover.unwrap();
        assert_eq!((cover.mime.as_str(), &cover.data[..]), ("image/png", &b"\x89PNG pic"[..]));
    }

    #[test]
    fn truncated_blocks_keep_what_came_before() {
        let mut b = comment_block(&["TITLE=Kept"]);
        b[5..9].copy_from_slice(&u32::MAX.to_le_bytes()); // claims 4 billion entries
        b.extend(50u32.to_le_bytes()); // an entry longer than what follows
        b.extend(b"ARTIST=short");
        let m = parsed(&b);
        assert_eq!((m.title.as_deref(), m.artist), (Some("Kept"), None));
        assert!(parsed(&[1, 2]).is_empty());
    }

    #[test]
    fn flac_picture_blocks() {
        let block = picture_block(3, "IMAGE/JPG", &[0xFF, 0xD8, 1]);
        let (front, mime, data) = parse_flac_picture(&block).unwrap();
        assert_eq!((front, mime.as_str(), data), (true, "image/jpeg", &[0xFF, 0xD8, 1][..]));
        assert!(!parse_flac_picture(&picture_block(4, "image/png", b"x")).unwrap().0, "type 4 is the back cover");
        assert!(parse_flac_picture(&picture_block(3, "image/png", b"data")[..30]).is_none());
    }

    #[test]
    fn base64_decodes_with_and_without_padding() {
        assert_eq!(base64("TWFu").unwrap(), b"Man");
        assert_eq!(base64("TWE=").unwrap(), b"Ma");
        assert_eq!(base64("TQ==\n").unwrap(), b"M");
        assert!(base64("T*E=").is_none());
    }
}
