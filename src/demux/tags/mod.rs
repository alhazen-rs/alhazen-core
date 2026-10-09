//! Tag readers: bytes → `Metadata` fields. Malformed input only leaves fields empty.

pub(crate) mod id3;
#[cfg(feature = "native")]
pub(crate) mod riff;
#[cfg(feature = "native")]
pub(crate) mod vorbis;

/// A forward cursor over tag bytes.
#[cfg(feature = "native")]
pub(crate) struct Bytes<'a> {
    b: &'a [u8],
    le: bool,
}

#[cfg(feature = "native")]
impl<'a> Bytes<'a> {
    pub fn le(b: &'a [u8]) -> Self {
        Self { b, le: true }
    }

    pub fn be(b: &'a [u8]) -> Self {
        Self { b, le: false }
    }

    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.b.len() {
            return None;
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Some(head)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    pub fn u32(&mut self) -> Option<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
}
