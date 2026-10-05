//! VP9 decoding via `vp9-mt` (our multi-threaded fork of rusty_vp9).

use std::time::Duration;

use super::planar::{double_rows, pack_8bit};
use super::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame};
use crate::demux::Packet;
use crate::{Error, Result};

pub struct Vp9Decoder {
    dec: vp9_mt::Vp9Decoder,
    threads: usize,
}

impl Vp9Decoder {
    /// `threads == 0` uses the available parallelism.
    pub fn new(threads: usize) -> Self {
        Self { dec: vp9_mt::Vp9Decoder::with_threads(threads), threads }
    }
}

impl VideoDecoder for Vp9Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.dec
            .push(&packet.data, Some(packet.pts.as_nanos() as i64))
            .map_err(|e| Error::Decode(format!("vp9: {e}")))
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        loop {
            match self.dec.next_picture() {
                Ok(pic) => return convert(&pic).map(|f| Some(DecodedFrame::Yuv(f))),
                // A hidden frame (alt-ref) was decoded; the shown one may still be queued.
                Err(vp9_mt::Error::Again) if self.dec.pending() > 0 => continue,
                Err(vp9_mt::Error::Again | vp9_mt::Error::Eof) => return Ok(None),
                Err(e) => return Err(Error::Decode(format!("vp9: {e}"))),
            }
        }
    }

    fn flush(&mut self) {
        self.dec = vp9_mt::Vp9Decoder::with_threads(self.threads);
    }
}

fn convert(pic: &vp9_mt::Picture) -> Result<YuvFrame> {
    let (w, h) = (pic.width(), pic.height());
    let bd = pic.bit_depth();
    let matrix = match pic.color_space() {
        1 | 3 => ColorMatrix::Bt601,
        2 => ColorMatrix::Bt709,
        5 => ColorMatrix::Bt2020,
        7 => return Err(Error::Decode("vp9: RGB (color_space 7) streams are not supported".into())),
        _ => ColorMatrix::guess_for_height(h),
    };
    let pack = |p: usize| {
        let (data, stride) = pic.plane(p);
        let (pw, ph) = pic.plane_size(p);
        (pack_8bit(data, stride, pw, ph, bd), pw)
    };
    let (y, _) = pack(0);
    let ((u, cw), (v, _)) = (pack(1), pack(2));
    let (layout, u, v) = match pic.subsampling() {
        (1, 1) => (PixelLayout::I420, u, v),
        (1, 0) => (PixelLayout::I422, u, v),
        (0, 0) => (PixelLayout::I444, u, v),
        // 4:4:0 (full-width, half-height chroma): repeat chroma rows to get 4:4:4.
        _ => (PixelLayout::I444, double_rows(&u, cw, h as usize), double_rows(&v, cw, h as usize)),
    };
    Ok(YuvFrame {
        width: w,
        height: h,
        layout,
        planes: [y, u, v],
        strides: [w as usize, cw, cw],
        matrix,
        full_range: pic.full_range(),
        pts: Duration::from_nanos(pic.pts.unwrap_or(0).max(0) as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Demuxer, MatroskaDemuxer, Mp4Demuxer};
    use crate::source::FileSource;

    fn decode_all(mut demuxer: Box<dyn Demuxer>) -> Vec<YuvFrame> {
        let mut dec = Vp9Decoder::new(2);
        let mut frames = vec![];
        while let Some(p) = demuxer.next_packet().unwrap() {
            dec.send_packet(&p).unwrap();
            while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
                frames.push(f);
            }
        }
        while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
            frames.push(f);
        }
        frames
    }

    fn webm(name: &str) -> Box<dyn Demuxer> {
        let src = Box::new(FileSource::open(format!("tests/fixtures/{name}")).unwrap());
        Box::new(MatroskaDemuxer::open(src).unwrap())
    }

    #[test]
    fn decodes_every_frame_of_profile0_webm() {
        let frames = decode_all(webm("vp9_profile0.webm"));
        assert_eq!(frames.len(), 60);
        let f = &frames[0];
        assert_eq!((f.width, f.height, f.layout), (320, 240, PixelLayout::I420));
        assert_eq!((f.planes[0].len(), f.planes[1].len()), (320 * 240, 160 * 120));
        assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts), "frames come out in display order");
        assert_eq!(frames[30].pts, Duration::from_secs(1));
    }

    #[test]
    fn ten_bit_is_shifted_to_eight() {
        let frames = decode_all(webm("vp9_10bit.webm"));
        assert_eq!(frames.len(), 60);
        // testsrc2's frame has bright and dark areas; 10-bit samples shifted by 2 span most of 0..=255.
        let y = &frames[0].planes[0];
        assert!(*y.iter().max().unwrap() > 200 && *y.iter().min().unwrap() < 40);
    }

    #[test]
    fn decodes_vp9_in_mp4() {
        let src = Box::new(FileSource::open("tests/fixtures/vp9.mp4").unwrap());
        assert_eq!(decode_all(Box::new(Mp4Demuxer::open(src).unwrap())).len(), 60);
    }

    #[test]
    fn flush_then_keyframe_decodes_again() {
        let mut demuxer = webm("vp9_profile0.webm");
        let packets: Vec<_> = std::iter::from_fn(|| demuxer.next_packet().unwrap()).collect();
        let mut dec = Vp9Decoder::new(2);
        dec.send_packet(&packets[0]).unwrap();
        assert!(dec.receive_frame().unwrap().is_some());
        dec.flush();
        let key = packets.iter().find(|p| p.keyframe && p.pts == Duration::from_secs(1)).unwrap();
        dec.send_packet(key).unwrap();
        assert_eq!(dec.receive_frame().unwrap().unwrap().pts(), Duration::from_secs(1));
    }

    #[test]
    fn garbage_packet_is_an_error_not_a_crash() {
        let mut dec = Vp9Decoder::new(2);
        let p = Packet { stream: 1, pts: Duration::ZERO, keyframe: true, data: vec![0xFF; 64], generation: 0 };
        let sent = dec.send_packet(&p);
        let received = dec.receive_frame();
        assert!(sent.is_err() || received.is_err() || matches!(received, Ok(None)));
    }
}
