//! Apple ProRes decoding via `oxideav-prores` (pure Rust, MIT). ProRes is intra-only, so every
//! packet decodes on its own. 10/12-bit output is requested as 8-bit; 4444 alpha is ignored.

use std::collections::VecDeque;
use std::time::Duration;

use oxideav_prores::frame::ChromaFormat;

use super::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame};
use crate::demux::Packet;
use crate::{Error, Result};

#[derive(Default)]
pub struct ProResDecoder {
    ready: VecDeque<YuvFrame>,
}

impl ProResDecoder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoDecoder for ProResDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.ready.push_back(decode(&packet.data, packet.pts)?);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        Ok(self.ready.pop_front().map(DecodedFrame::Yuv))
    }

    fn flush(&mut self) {
        self.ready.clear();
    }
}

fn decode(data: &[u8], pts: Duration) -> Result<YuvFrame> {
    fn err(e: impl std::fmt::Display) -> Error {
        Error::Decode(format!("prores: {e}"))
    }
    // Packets in MOV are bare `icpf` frames; Matroska V_PRORES packets lack the 8-byte
    // size + 'icpf' prefix (Matroska codec mapping), so restore it.
    let owned;
    let data = if data.get(4..8) == Some(b"icpf") {
        data
    } else {
        let mut v = Vec::with_capacity(data.len() + 8);
        v.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(b"icpf");
        v.extend_from_slice(data);
        owned = v;
        &owned
    };
    let (header, _) = oxideav_prores::frame::parse_frame(data).map_err(err)?;
    let (w, h) = (header.width as usize, header.height as usize);
    let interlaced = header.interlace_mode != 0;
    // oxideav crops its padded planes before returning, which would cut off the 4:4:4 chroma
    // blocks we have to move back from below the picture in a partial last macroblock row.
    // Declare a padded height (same macroblock rows) so nothing is cropped away; crop below.
    let padded_h = match header.chroma_format {
        ChromaFormat::Y444 => padded_height(h, interlaced),
        ChromaFormat::Y422 => h,
    };
    let patched;
    let data = if padded_h != h {
        let mut v = data.to_vec();
        v[18..20].copy_from_slice(&(padded_h as u16).to_be_bytes()); // frame header vertical_size
        patched = v;
        &patched
    } else {
        data
    };
    let frame = oxideav_prores::decoder::decode_packet(data, None).map_err(err)?;
    let (layout, cw) = match header.chroma_format {
        ChromaFormat::Y422 => (PixelLayout::I422, w.div_ceil(2)),
        ChromaFormat::Y444 => (PixelLayout::I444, w),
    };
    let mut frame = frame;
    let planes = &mut frame.planes;
    if planes.len() < 3 {
        return Err(Error::Decode(format!("prores: expected 3 planes, got {}", planes.len())));
    }
    if layout == PixelLayout::I444 {
        for p in &mut planes[1..3] {
            fix_444_chroma_block_order(&mut p.data, p.stride, interlaced);
        }
    }
    let planes: Vec<(&[u8], usize)> = planes.iter().take(3).map(|p| (&p.data[..], p.stride)).collect();
    let crop = |(data, stride): (&[u8], usize), pw: usize| -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(pw * h);
        for row in 0..h {
            let line = data.get(row * stride..row * stride + pw);
            out.extend_from_slice(line.ok_or_else(|| Error::Decode("prores: plane smaller than frame".into()))?);
        }
        Ok(out)
    };
    let matrix = match header.matrix_coefficients {
        1 => ColorMatrix::Bt709,
        5 | 6 => ColorMatrix::Bt601,
        9 => ColorMatrix::Bt2020,
        _ => ColorMatrix::guess_for_height(h as u32),
    };
    Ok(YuvFrame {
        width: w as u32,
        height: h as u32,
        layout,
        planes: [crop(planes[0], w)?, crop(planes[1], cw)?, crop(planes[2], cw)?],
        strides: [w, cw, cw],
        matrix,
        full_range: false,
        pts,
    })
}

/// The height, rounded up to whole macroblocks (16 rows; per field when interlaced), that keeps
/// the number of macroblock rows — and so the bitstream's layout — unchanged. Returns `height`
/// itself when no such padding exists (odd interlaced heights whose fields differ in rows).
fn padded_height(height: usize, interlaced: bool) -> usize {
    if !interlaced {
        return height.div_ceil(16) * 16;
    }
    let rows = |field: usize| field.div_ceil(16);
    let padded = height.div_ceil(32) * 32;
    let same = rows(height.div_ceil(2)) == rows(padded / 2) && rows(height / 2) == rows(padded / 2);
    if same { padded } else { height }
}

/// oxideav-prores 0.1.1 writes the second and third 8x8 chroma blocks of each 4:4:4 macroblock
/// to swapped positions (top-right <-> bottom-left); ffmpeg and Apple's order is raster (RDD 36
/// §7.1.2). Swap them back across the whole padded 8-bit plane. Interlaced frames are decoded as
/// two fields interleaved by row, so macroblocks are formed from every other row there.
fn fix_444_chroma_block_order(plane: &mut [u8], stride: usize, interlaced: bool) {
    if stride == 0 {
        return;
    }
    let rows = plane.len() / stride;
    let fields: &[(usize, usize)] = if interlaced { &[(0, 2), (1, 2)] } else { &[(0, 1)] };
    for &(first, step) in fields {
        let field_rows = (rows - first).div_ceil(step);
        // Row `r` of this field is plane row `first + r * step`.
        for my in (0..field_rows.saturating_sub(15)).step_by(16) {
            for mx in (0..stride.saturating_sub(15)).step_by(16) {
                for y in 0..8 {
                    let top = (first + (my + y) * step) * stride + mx + 8;
                    let bottom = (first + (my + 8 + y) * step) * stride + mx;
                    for x in 0..8 {
                        plane.swap(top + x, bottom + x);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{Demuxer, Mp4Demuxer};
    use crate::source::FileSource;

    fn decode_file(name: &str) -> Vec<YuvFrame> {
        let src = Box::new(FileSource::open(format!("tests/fixtures/{name}")).unwrap());
        let mut d = Mp4Demuxer::open(src).unwrap();
        let mut dec = ProResDecoder::new();
        let mut frames = vec![];
        while let Some(p) = d.next_packet().unwrap() {
            dec.send_packet(&p).unwrap();
            while let Some(DecodedFrame::Yuv(f)) = dec.receive_frame().unwrap() {
                frames.push(f);
            }
        }
        frames
    }

    #[test]
    fn decodes_prores_422_hq() {
        let frames = decode_file("prores_hq.mov");
        assert_eq!(frames.len(), 6);
        let f = &frames[0];
        assert_eq!((f.width, f.height, f.layout), (192, 128, PixelLayout::I422));
        assert_eq!((f.planes[0].len(), f.planes[1].len()), (192 * 128, 96 * 128));
        assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts));
    }

    #[test]
    fn decodes_prores_4444() {
        let frames = decode_file("prores_4444.mov");
        assert_eq!(frames.len(), 6);
        assert_eq!(frames[0].layout, PixelLayout::I444);
        assert_eq!(frames[0].planes[2].len(), 192 * 128);
    }

    #[test]
    fn matroska_style_packet_without_icpf_prefix_decodes() {
        let src = Box::new(FileSource::open("tests/fixtures/prores_hq.mov").unwrap());
        let p = Mp4Demuxer::open(src).unwrap().next_packet().unwrap().unwrap();
        let bare = &p.data[8..];
        assert_eq!(decode(bare, Duration::ZERO).unwrap().width, 192);
    }

    #[test]
    fn block_order_fix_swaps_top_right_and_bottom_left() {
        // One 16x16 macroblock whose four 8x8 blocks hold 0, 1, 2, 3 in the decoder's order.
        let mut plane = vec![0u8; 256];
        for y in 0..16 {
            for x in 0..16 {
                plane[y * 16 + x] = [[0, 2], [1, 3]][y / 8][x / 8];
            }
        }
        fix_444_chroma_block_order(&mut plane, 16, false);
        assert_eq!((plane[0], plane[8], plane[8 * 16], plane[8 * 16 + 8]), (0, 1, 2, 3));
    }

    #[test]
    fn padding_keeps_macroblock_rows() {
        assert_eq!(padded_height(1080, false), 1088);
        assert_eq!(padded_height(128, false), 128);
        assert_eq!(padded_height(1080, true), 1088, "540-row fields become 544");
        assert_eq!(padded_height(120, true), 128);
        assert_eq!(padded_height(33, true), 33, "fields of 17 and 16 rows cannot share a padding");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode(&[0u8; 64], Duration::ZERO).is_err());
    }
}
