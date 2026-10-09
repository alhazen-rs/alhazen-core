//! iTunes-style MP4 tags: `moov/udta/meta/ilst`.

use crate::demux::metadata::{CoverPick, Field, Metadata, sniff_mime};

/// The boxes in `b`: (type, body).
fn boxes(mut b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let (head, size) = match size {
            0 => (8, b.len()),
            1 if b.len() >= 16 => (16, u64::from_be_bytes(b[8..16].try_into().ok()?) as usize),
            s => (8, s),
        };
        if size < head || size > b.len() {
            return None;
        }
        let item = (&b[4..8], &b[head..size]);
        b = &b[size..];
        Some(item)
    })
}

fn child<'a>(b: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
    boxes(b).find(|(k, _)| *k == kind).map(|(_, body)| body)
}

/// Tags from `moov`'s body: `udta/meta/ilst` items with their `data` boxes.
pub(crate) fn parse_moov_tags(moov: &[u8], meta: &mut Metadata) {
    let Some(meta_box) = child(moov, b"udta").and_then(|u| child(u, b"meta")) else { return };
    // ISO's `meta` is a full box (4 bytes of version and flags); QuickTime's is not.
    let body = if meta_box.get(4..8) == Some(b"hdlr") { meta_box } else { meta_box.get(4..).unwrap_or(&[]) };
    let Some(ilst) = child(body, b"ilst") else { return };
    let mut covers = CoverPick::default();
    for (kind, item) in boxes(ilst) {
        for (_, data) in boxes(item).filter(|(k, _)| *k == b"data") {
            let Some(value) = data.get(8..) else { continue };
            let type_code = u32::from_be_bytes([0, data[1], data[2], data[3]]);
            let text = || String::from_utf8_lossy(value).into_owned();
            match kind {
                b"\xA9nam" => meta.set(Field::Title, &text()),
                b"\xA9ART" => meta.set(Field::Artist, &text()),
                b"\xA9alb" => meta.set(Field::Album, &text()),
                b"aART" => meta.set(Field::AlbumArtist, &text()),
                b"\xA9day" => meta.set(Field::Year, &text()),
                b"\xA9gen" => meta.set(Field::Genre, &text()),
                b"gnre" if value.len() >= 2 => {
                    let n = u16::from_be_bytes([value[0], value[1]]) as usize;
                    if let Some(g) = n.checked_sub(1).and_then(|i| super::id3::GENRES.get(i)) {
                        meta.set(Field::Genre, g);
                    }
                }
                b"trkn" if value.len() >= 4 => {
                    let n = u16::from_be_bytes([value[2], value[3]]);
                    if n > 0 {
                        meta.set(Field::Track, &n.to_string());
                    }
                }
                b"covr" => {
                    let mime = match type_code {
                        13 => "image/jpeg".to_owned(),
                        14 => "image/png".to_owned(),
                        _ => sniff_mime(value),
                    };
                    covers.offer(true, &mime, value); // `covr` has no picture type: the first is the cover
                }
                _ => {}
            }
        }
    }
    covers.finish(meta);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8], body: &[u8]) -> Vec<u8> {
        let mut a = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        a.extend(kind);
        a.extend(body);
        a
    }

    fn data(type_code: u32, value: &[u8]) -> Vec<u8> {
        atom(b"data", &[&type_code.to_be_bytes()[..], &[0; 4], value].concat())
    }

    #[test]
    fn ilst_items_including_numeric_genre_and_cover() {
        let ilst = [
            atom(b"\xA9nam", &data(1, b"Song")),
            atom(b"\xA9ART", &data(1, b"Band")),
            atom(b"trkn", &data(0, &[0, 0, 0, 5, 0, 9, 0, 0])),
            atom(b"gnre", &data(0, &[0, 18])), // ID3 genre + 1 → Rock
            atom(b"covr", &data(13, &[0xFF, 0xD8, 1])),
        ]
        .concat();
        let meta_box = [&[0u8; 4][..], &atom(b"hdlr", &[0; 25]), &atom(b"ilst", &ilst)].concat(); // ISO full box
        let moov = atom(b"udta", &atom(b"meta", &meta_box));
        let mut m = Metadata::default();
        parse_moov_tags(&moov, &mut m);
        assert_eq!((m.title.as_deref(), m.artist.as_deref(), m.track, m.genre.as_deref()), (Some("Song"), Some("Band"), Some(5), Some("Rock")));
        assert_eq!(m.cover.unwrap().mime, "image/jpeg");
    }
}
