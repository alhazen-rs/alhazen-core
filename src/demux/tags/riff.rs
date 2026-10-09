//! RIFF `LIST`/`INFO` tags (WAV).

use crate::demux::metadata::{Field, Metadata};

/// The sub-chunks of a RIFF `LIST` chunk of type `INFO` (`body` follows the `INFO` fourcc).
#[allow(dead_code)] // first used by a reader (Task 4/5)
pub(crate) fn parse_riff_info(body: &[u8], meta: &mut Metadata) {
    let mut pos = 0usize;
    while let Some(head) = body.get(pos..pos + 8) {
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let Some(data) = body.get(pos + 8..pos + 8 + size) else { return };
        let field = match &head[..4] {
            b"INAM" => Some(Field::Title),
            b"IART" => Some(Field::Artist),
            b"IPRD" => Some(Field::Album),
            b"IGNR" => Some(Field::Genre),
            b"ICRD" => Some(Field::Year),
            b"ITRK" | b"IPRT" => Some(Field::Track),
            _ => None,
        };
        if let Some(field) = field {
            let text = data.split(|&c| c == 0).next().unwrap_or(&[]);
            meta.set(field, &String::from_utf8_lossy(text));
        }
        pos += 8 + size + (size & 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::metadata::Metadata;

    #[test]
    fn info_chunks_with_odd_sizes() {
        let mut b = Vec::new();
        for (id, value) in [(b"INAM", "Song\0"), (b"IART", "Band"), (b"ITRK", "4\0"), (b"ICRD", "2021"), (b"IGNR", "Pop"), (b"IPRD", "Odd")] {
            b.extend(id);
            b.extend((value.len() as u32).to_le_bytes());
            b.extend(value.as_bytes());
            if value.len() % 2 == 1 {
                b.push(0); // pad byte
            }
        }
        let mut m = Metadata::default();
        parse_riff_info(&b, &mut m);
        assert_eq!((m.title.as_deref(), m.artist.as_deref(), m.album.as_deref()), (Some("Song"), Some("Band"), Some("Odd")));
        assert_eq!((m.track, m.year, m.genre.as_deref()), (Some(4), Some(2021), Some("Pop")));
        let mut m = Metadata::default();
        parse_riff_info(b"INAM\xFF\x00\x00\x00abc", &mut m); // longer than the chunk
        assert!(m.is_empty());
    }
}
