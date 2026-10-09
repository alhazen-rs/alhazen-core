//! Tags and cover art of a file, as read by the demuxers.

use std::sync::Arc;

/// Title, artist, album and the other common tags, plus the cover. Every field is optional.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    /// The front cover, else the first picture.
    pub cover: Option<Picture>,
}

/// An embedded picture, still encoded (JPEG/PNG).
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub mime: String,
    pub data: Arc<[u8]>,
}

/// Pictures larger than this are skipped.
pub(crate) const MAX_PICTURE: usize = 16 << 20;
/// Text fields longer than this are ignored.
pub(crate) const MAX_TEXT: usize = 64 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Track,
    Year,
    Genre,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        *self == Metadata::default()
    }

    /// Sets `field` from tag text. The first value wins; blank or oversized text is ignored.
    /// Track numbers accept `3` and `3/12`; years take the first four digits (`2024-05-01`).
        pub(crate) fn set(&mut self, field: Field, value: &str) {
        let v = value.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if v.is_empty() || v.len() > MAX_TEXT {
            return;
        }
        let text = |slot: &mut Option<String>| {
            if slot.is_none() {
                *slot = Some(v.to_owned());
            }
        };
        match field {
            Field::Title => text(&mut self.title),
            Field::Artist => text(&mut self.artist),
            Field::Album => text(&mut self.album),
            Field::AlbumArtist => text(&mut self.album_artist),
            Field::Genre => text(&mut self.genre),
            Field::Track => {
                if self.track.is_none() {
                    self.track = v.split('/').next().and_then(|n| n.trim().parse().ok()).filter(|&n| n > 0);
                }
            }
            Field::Year => {
                if self.year.is_none() {
                    self.year = v.get(..4).and_then(|y| y.parse().ok());
                }
            }
        }
    }
}

/// Chooses the cover among a file's pictures: the front cover, else the first one offered.
#[derive(Default)]
pub(crate) struct CoverPick {
    pic: Option<Picture>,
    front: bool,
}

impl CoverPick {
    pub fn offer(&mut self, front: bool, mime: &str, data: &[u8]) {
        if data.is_empty() || data.len() > MAX_PICTURE || self.front || (self.pic.is_some() && !front) {
            return;
        }
        let mime = if mime.is_empty() || mime == "image/jpg" { sniff_mime(data) } else { mime.to_ascii_lowercase() };
        self.pic = Some(Picture { mime, data: data.into() });
        self.front = front;
    }

    pub fn finish(self, meta: &mut Metadata) {
        if meta.cover.is_none() {
            meta.cover = self.pic;
        }
    }
}

/// MIME type from an image's magic bytes.
pub(crate) fn sniff_mime(data: &[u8]) -> String {
    if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else {
        "application/octet-stream"
    }
    .to_owned()
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn first_value_wins_and_numbers_are_parsed() {
        let mut m = Metadata::default();
        m.set(Field::Title, "  First \0");
        m.set(Field::Title, "Second");
        m.set(Field::Track, "3/12");
        m.set(Field::Year, "2024-05-01");
        m.set(Field::Genre, "   ");
        assert_eq!(m.title.as_deref(), Some("First"));
        assert_eq!((m.track, m.year, m.genre.as_deref()), (Some(3), Some(2024), None));
        m.set(Field::Artist, &"x".repeat(MAX_TEXT + 1));
        assert_eq!(m.artist, None, "oversized text is ignored");
    }

    #[test]
    fn the_front_cover_wins_over_earlier_pictures() {
        let mut pick = CoverPick::default();
        pick.offer(false, "image/png", b"\x89PNG back");
        pick.offer(true, "", &[0xFF, 0xD8, 1]);
        pick.offer(false, "image/png", b"\x89PNG later");
        let mut m = Metadata::default();
        pick.finish(&mut m);
        let cover = m.cover.unwrap();
        assert_eq!((cover.mime.as_str(), &cover.data[..]), ("image/jpeg", &[0xFF, 0xD8, 1][..]));
    }
}
