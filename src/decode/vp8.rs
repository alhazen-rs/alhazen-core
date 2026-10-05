//! VP8 decoding via `oximedia-codec` (pure Rust, RFC 6386; only its `vp8` feature is enabled).

use std::time::Duration;

use oximedia_codec::traits::{DecoderConfig, VideoDecoder as _};
use oximedia_codec::vp8::Vp8Decoder as Inner;

use super::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame};
use crate::demux::Packet;
use crate::{Error, Result};

pub struct Vp8Decoder {
    inner: Inner,
}

impl Vp8Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self { inner: new_inner()? })
    }
}

fn new_inner() -> Result<Inner> {
    Inner::new(DecoderConfig::default()).map_err(|e| Error::Decode(format!("vp8: {e}")))
}

impl VideoDecoder for Vp8Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.inner
            .send_packet(&packet.data, packet.pts.as_nanos() as i64)
            .map_err(|e| Error::Decode(format!("vp8: {e}")))
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        let Some(f) = self.inner.receive_frame().map_err(|e| Error::Decode(format!("vp8: {e}")))? else {
            return Ok(None);
        };
        let [y, u, v] = &f.planes[..] else {
            return Err(Error::Decode(format!("vp8: expected 3 planes, got {}", f.planes.len())));
        };
        let pack = |p: &oximedia_codec::frame::Plane| -> Vec<u8> {
            let w = p.width as usize;
            (0..p.height as usize).flat_map(|row| &p.data[row * p.stride..row * p.stride + w]).copied().collect()
        };
        Ok(Some(DecodedFrame::Yuv(YuvFrame {
            width: f.width,
            height: f.height,
            layout: PixelLayout::I420,
            planes: [pack(y), pack(u), pack(v)],
            strides: [y.width as usize, u.width as usize, v.width as usize],
            // VP8 is always BT.601 studio range (RFC 6386 §9.2: color_space 0 is the only one defined).
            matrix: ColorMatrix::Bt601,
            full_range: false,
            pts: Duration::from_nanos(f.timestamp.pts.max(0) as u64),
        })))
    }

    fn flush(&mut self) {
        self.inner.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Demuxer, MatroskaDemuxer};
    use crate::source::FileSource;

    fn packets() -> Vec<Packet> {
        let src = Box::new(FileSource::open("tests/fixtures/vp8.webm").unwrap());
        let mut d = MatroskaDemuxer::open(src).unwrap();
        std::iter::from_fn(|| d.next_packet().unwrap()).collect()
    }

    #[test]
    fn decodes_every_frame_of_webm() {
        let mut dec = Vp8Decoder::new().unwrap();
        let mut frames = vec![];
        for p in packets() {
            dec.send_packet(&p).unwrap();
            while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
                frames.push(f);
            }
        }
        assert_eq!(frames.len(), 60);
        let f = &frames[0];
        assert_eq!((f.width, f.height, f.layout), (320, 240, PixelLayout::I420));
        assert_eq!((f.planes[0].len(), f.planes[1].len()), (320 * 240, 160 * 120));
        assert_eq!(frames[30].pts, Duration::from_secs(1));
    }

    #[test]
    fn flush_then_keyframe_decodes_again() {
        let packets = packets();
        let mut dec = Vp8Decoder::new().unwrap();
        dec.send_packet(&packets[0]).unwrap();
        dec.send_packet(&packets[1]).unwrap();
        dec.flush();
        assert!(dec.receive_frame().unwrap().is_none(), "flush drops queued frames");
        let key = packets.iter().find(|p| p.keyframe && p.pts == Duration::from_secs(1)).unwrap();
        dec.send_packet(key).unwrap();
        assert_eq!(dec.receive_frame().unwrap().unwrap().pts(), Duration::from_secs(1));
    }

    #[test]
    fn inter_frame_after_flush_is_an_error_not_a_crash() {
        let packets = packets();
        let mut dec = Vp8Decoder::new().unwrap();
        assert!(dec.send_packet(&packets[1]).is_err());
    }
}
