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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_length_includes_header_and_footer() {
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x02\x01"), Some(10 + 257));
        assert_eq!(id3v2_len(b"ID3\x04\x00\x10\x00\x00\x00\x05"), Some(10 + 5 + 10));
        assert_eq!(id3v2_len(b"ID3\x04\x00\x00\x00\x00\x80\x00"), None, "not synchsafe");
        assert_eq!(id3v2_len(b"TAG"), None);
    }
}
