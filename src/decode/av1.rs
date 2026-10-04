//! AV1 decoding via `rav1d` (pure Rust port of dav1d), using its dav1d-compatible API.

use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::time::Duration;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_MC_BT709, DAV1D_MC_BT2020_CL, DAV1D_MC_BT2020_NCL, DAV1D_MC_BT470BG, DAV1D_MC_BT601,
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
    dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
};

use super::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, chroma_size};
use crate::demux::Packet;
use crate::{Error, Result};

const EAGAIN: i32 = -libc::EAGAIN;

pub struct Av1Decoder {
    ctx: Option<Dav1dContext>,
    /// Pictures pulled out while making room for input (dav1d returned EAGAIN on send).
    ready: VecDeque<DecodedFrame>,
}

// SAFETY: the dav1d context is only ever used through `&mut self`, i.e. from one thread at a time.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    /// `threads == 0` lets rav1d pick based on the CPU count.
    pub fn new(threads: usize) -> Result<Self> {
        let mut ctx: Option<Dav1dContext> = None;
        // SAFETY: settings are initialized by `dav1d_default_settings` before use; pointers are valid.
        let result = unsafe {
            let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
            dav1d_default_settings(NonNull::new(settings.as_mut_ptr()).unwrap());
            let mut settings = settings.assume_init();
            settings.n_threads = threads.min(256) as i32;
            dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut settings))
        };
        if result.0 != 0 || ctx.is_none() {
            return Err(Error::Decode(format!("dav1d_open failed ({})", result.0)));
        }
        Ok(Self { ctx, ready: VecDeque::new() })
    }

    fn get_picture(&mut self) -> Result<Option<DecodedFrame>> {
        // SAFETY: a zeroed Dav1dPicture is the documented "empty" value; ctx is open.
        unsafe {
            let mut pic: Dav1dPicture = std::mem::zeroed();
            let r = dav1d_get_picture(self.ctx, NonNull::new(&mut pic)).0;
            if r == EAGAIN {
                return Ok(None);
            }
            if r != 0 {
                return Err(Error::Decode(format!("dav1d_get_picture failed ({r})")));
            }
            let frame = copy_picture(&pic);
            dav1d_picture_unref(NonNull::new(&mut pic));
            Ok(Some(DecodedFrame::Yuv(frame)))
        }
    }
}

impl VideoDecoder for Av1Decoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        // SAFETY: `dav1d_data_create` allocates `len` bytes that we fully initialize; the data is
        // either consumed by dav1d or released with `dav1d_data_unref`.
        unsafe {
            let mut data: Dav1dData = std::mem::zeroed();
            let buf = dav1d_data_create(NonNull::new(&mut data), packet.data.len());
            if buf.is_null() {
                return Err(Error::Decode("dav1d_data_create failed".into()));
            }
            std::ptr::copy_nonoverlapping(packet.data.as_ptr(), buf, packet.data.len());
            data.m.timestamp = packet.pts.as_nanos() as i64;
            loop {
                let r = dav1d_send_data(self.ctx, NonNull::new(&mut data)).0;
                if r == EAGAIN {
                    // Decoder is full: drain a picture, then retry with the remaining data.
                    match self.get_picture() {
                        Ok(Some(f)) => self.ready.push_back(f),
                        Ok(None) => {}
                        Err(e) => {
                            dav1d_data_unref(NonNull::new(&mut data));
                            return Err(e);
                        }
                    }
                    continue;
                }
                if data.sz > 0 {
                    dav1d_data_unref(NonNull::new(&mut data));
                }
                return if r == 0 {
                    Ok(())
                } else {
                    Err(Error::Decode(format!("dav1d_send_data failed ({r})")))
                };
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        if let Some(f) = self.ready.pop_front() {
            return Ok(Some(f));
        }
        self.get_picture()
    }

    fn flush(&mut self) {
        self.ready.clear();
        if let Some(ctx) = self.ctx {
            // SAFETY: ctx is open.
            unsafe { dav1d_flush(ctx) };
        }
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        // SAFETY: ctx came from dav1d_open and is closed exactly once.
        unsafe { dav1d_close(NonNull::new(&mut self.ctx)) };
    }
}

/// Copies a dav1d picture into an owned 8-bit `YuvFrame`.
///
/// # Safety
/// `pic` must be a valid picture returned by `dav1d_get_picture`.
unsafe fn copy_picture(pic: &Dav1dPicture) -> YuvFrame {
    let (w, h) = (pic.p.w as u32, pic.p.h as u32);
    let bpc = pic.p.bpc as u32;
    let layout = match pic.p.layout {
        DAV1D_PIXEL_LAYOUT_I400 => PixelLayout::I400,
        DAV1D_PIXEL_LAYOUT_I420 => PixelLayout::I420,
        DAV1D_PIXEL_LAYOUT_I422 => PixelLayout::I422,
        _ => PixelLayout::I444,
    };
    // SAFETY: seq_hdr is set on every output picture.
    let seq = unsafe { pic.seq_hdr.map(|p| p.as_ref()) };
    let matrix = match seq.map(|s| s.mtrx) {
        Some(DAV1D_MC_BT709) => ColorMatrix::Bt709,
        Some(DAV1D_MC_BT2020_NCL | DAV1D_MC_BT2020_CL) => ColorMatrix::Bt2020,
        Some(DAV1D_MC_BT601 | DAV1D_MC_BT470BG) => ColorMatrix::Bt601,
        _ => ColorMatrix::guess_for_height(h),
    };
    let full_range = seq.is_some_and(|s| s.color_range != 0);
    let (cw, ch) = chroma_size(layout, w, h);
    let dims = [(w, h), (cw, ch), (cw, ch)];
    let mut planes: [Vec<u8>; 3] = Default::default();
    for (i, plane) in planes.iter_mut().enumerate() {
        let (pw, ph) = dims[i];
        let Some(src) = pic.data[i] else { continue };
        if pw == 0 {
            continue;
        }
        let stride = pic.stride[(i > 0) as usize];
        plane.reserve_exact((pw * ph) as usize);
        for row in 0..ph as isize {
            // SAFETY: dav1d guarantees `ph` rows of `stride` bytes, each with `pw` samples.
            unsafe {
                let row_ptr = (src.as_ptr() as *const u8).offset(row * stride);
                if bpc > 8 {
                    let samples = std::slice::from_raw_parts(row_ptr as *const u16, pw as usize);
                    plane.extend(samples.iter().map(|s| (s >> (bpc - 8)) as u8));
                } else {
                    plane.extend_from_slice(std::slice::from_raw_parts(row_ptr, pw as usize));
                }
            }
        }
    }
    YuvFrame {
        width: w,
        height: h,
        layout,
        planes,
        strides: [w as usize, cw as usize, cw as usize],
        matrix,
        full_range,
        pts: Duration::from_nanos(pic.m.timestamp.max(0) as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Demuxer, MatroskaDemuxer, Mp4Demuxer};
    use crate::source::FileSource;

    fn decode_all(mut demuxer: Box<dyn Demuxer>) -> Vec<YuvFrame> {
        let mut dec = Av1Decoder::new(2).unwrap();
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

    #[test]
    fn decodes_every_frame_of_webm() {
        let src = Box::new(FileSource::open("tests/fixtures/av1.webm").unwrap());
        let frames = decode_all(Box::new(MatroskaDemuxer::open(src).unwrap()));
        assert_eq!(frames.len(), 60);
        let f = &frames[0];
        assert_eq!((f.width, f.height, f.layout), (320, 240, PixelLayout::I420));
        assert_eq!(f.planes[0].len(), 320 * 240);
        assert_eq!(f.planes[1].len(), 160 * 120);
        assert_eq!(f.matrix, ColorMatrix::Bt601);
        assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts), "frames come out in display order");
    }

    #[test]
    fn decodes_every_frame_of_mp4() {
        let src = Box::new(FileSource::open("tests/fixtures/av1.mp4").unwrap());
        assert_eq!(decode_all(Box::new(Mp4Demuxer::open(src).unwrap())).len(), 60);
    }

    #[test]
    fn garbage_packet_is_an_error_not_a_crash() {
        let mut dec = Av1Decoder::new(1).unwrap();
        let p = Packet {
            stream: 1,
            pts: Duration::ZERO,
            keyframe: true,
            data: vec![0xFF; 64],
            generation: 0,
        };
        let sent = dec.send_packet(&p);
        let received = dec.receive_frame();
        assert!(sent.is_err() || received.is_err() || matches!(received, Ok(None)));
    }
}
