//! H.264/HEVC packaging: length-prefixed NAL units (MP4 `avcC`/`hvcC`, Matroska) to Annex B
//! start-code streams, as hardware decoders such as Media Foundation's expect.

use crate::{Error, Result};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Which configuration record the codec setup data is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamSetFormat {
    /// H.264 `AVCDecoderConfigurationRecord`.
    Avcc,
    /// HEVC `HEVCDecoderConfigurationRecord`.
    Hvcc,
}

/// Converts one stream's packets to Annex B.
#[derive(Clone, Debug)]
pub struct AnnexB {
    /// Bytes in each NAL unit's length prefix (1, 2 or 4).
    length_size: usize,
    /// The parameter sets (SPS/PPS, plus VPS for HEVC) as Annex B, sent before every keyframe
    /// so decoding can start at any of them (seeks).
    header: Vec<u8>,
}

impl AnnexB {
    /// From the configuration record (`StreamInfo::extradata`); `None` if it is malformed.
    pub fn from_config(format: ParamSetFormat, config: &[u8]) -> Option<AnnexB> {
        let (length_size, sets) = match format {
            ParamSetFormat::Avcc => parse_avcc(config)?,
            ParamSetFormat::Hvcc => parse_hvcc(config)?,
        };
        if !matches!(length_size, 1 | 2 | 4) {
            return None;
        }
        let mut header = Vec::new();
        for set in sets {
            header.extend_from_slice(&START_CODE);
            header.extend_from_slice(set);
        }
        Some(AnnexB { length_size, header })
    }

    /// Appends `packet` as Annex B to `out`, preceded by the parameter sets for keyframes.
    pub fn convert(&self, packet: &[u8], keyframe: bool, out: &mut Vec<u8>) -> Result<()> {
        if keyframe {
            out.extend_from_slice(&self.header);
        }
        let mut pos = 0;
        while pos < packet.len() {
            let len_bytes = packet
                .get(pos..pos + self.length_size)
                .ok_or_else(|| Error::Decode("NAL length prefix truncated".into()))?;
            let len = len_bytes.iter().fold(0usize, |n, &b| n << 8 | b as usize);
            pos += self.length_size;
            let nal = packet.get(pos..pos + len).ok_or_else(|| Error::Decode("NAL unit truncated".into()))?;
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
            pos += len;
        }
        Ok(())
    }
}

/// `AVCDecoderConfigurationRecord`: length size and SPS/PPS NAL units.
fn parse_avcc(c: &[u8]) -> Option<(usize, Vec<&[u8]>)> {
    if *c.first()? != 1 {
        return None;
    }
    let length_size = (*c.get(4)? & 3) as usize + 1;
    let mut sets = Vec::new();
    let mut pos = 5;
    for mask in [0x1F, 0xFF] {
        // numOfSequenceParameterSets (low 5 bits), then numOfPictureParameterSets.
        let count = (*c.get(pos)? & mask) as usize;
        pos += 1;
        for _ in 0..count {
            let len = u16::from_be_bytes(c.get(pos..pos + 2)?.try_into().ok()?) as usize;
            sets.push(c.get(pos + 2..pos + 2 + len)?);
            pos += 2 + len;
        }
    }
    Some((length_size, sets))
}

/// `HEVCDecoderConfigurationRecord`: length size and VPS/SPS/PPS (and SEI) NAL units.
fn parse_hvcc(c: &[u8]) -> Option<(usize, Vec<&[u8]>)> {
    let length_size = (*c.get(21)? & 3) as usize + 1;
    let arrays = *c.get(22)? as usize;
    let mut sets = Vec::new();
    let mut pos = 23;
    for _ in 0..arrays {
        let count = u16::from_be_bytes(c.get(pos + 1..pos + 3)?.try_into().ok()?) as usize;
        pos += 3;
        for _ in 0..count {
            let len = u16::from_be_bytes(c.get(pos..pos + 2)?.try_into().ok()?) as usize;
            sets.push(c.get(pos + 2..pos + 2 + len)?);
            pos += 2 + len;
        }
    }
    Some((length_size, sets))
}

/// The NAL units of an Annex B byte stream (start codes removed, trailing zero bytes dropped).
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        let mut nal = &data[start..end];
        while let [rest @ .., 0] = nal {
            nal = rest;
        }
        if !nal.is_empty() {
            nals.push(nal);
        }
    }
    nals
}

/// Removes emulation-prevention bytes (`00 00 03` → `00 00`).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb codes.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |v, _| {
            let bit = (self.data.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Some(v << 1 | bit as u32)
        })
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)? as u64) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// What an H.264 SPS says about the pictures.
struct H264Sps {
    width: u32,
    height: u32,
    chroma_format_idc: u32,
    bit_depth_luma_minus8: u32,
    bit_depth_chroma_minus8: u32,
    /// Pictures that can precede a picture in decode order and follow it in display order.
    max_num_reorder_frames: u32,
}

fn parse_h264_sps(sps: &[u8]) -> Option<H264Sps> {
    let data = rbsp(sps.get(1..)?);
    let mut b = Bits { data: &data, pos: 0 };
    let profile = b.u(8)?;
    b.skip(16)?; // constraint flags, level
    b.ue()?; // seq_parameter_set_id
    let (mut chroma, mut separate, mut depth_luma, mut depth_chroma) = (1, 0, 0, 0);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = b.ue()?;
        if chroma == 3 {
            separate = b.u(1)?;
        }
        depth_luma = b.ue()?;
        depth_chroma = b.ue()?;
        b.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if b.u(1)? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.u(1)? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1)?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = b.ue()?;
    b.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = b.ue()? + 1;
    let height_units = b.ue()? + 1;
    let frame_mbs_only = b.u(1)?;
    if frame_mbs_only == 0 {
        b.skip(1)?; // mb_adaptive_frame_field_flag
    }
    b.skip(1)?; // direct_8x8_inference_flag
    let mut width = width_mbs * 16;
    let mut height = (2 - frame_mbs_only) * height_units * 16;
    if b.u(1)? == 1 {
        let (left, right, top, bottom) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (crop_x, crop_y) = if separate == 1 || chroma == 0 {
            (1, 2 - frame_mbs_only)
        } else {
            (if chroma == 3 { 1 } else { 2 }, if chroma == 1 { 2 } else { 1 } * (2 - frame_mbs_only))
        };
        width = width.checked_sub(crop_x * (left + right))?;
        height = height.checked_sub(crop_y * (top + bottom))?;
    }
    // Without the VUI's bitstream restriction: none for Baseline (no B-frames), else at most the
    // reference frames.
    let fallback = if profile == 66 { 0 } else { max_num_ref_frames.min(16) };
    let max_num_reorder_frames = if b.u(1) == Some(1) { h264_vui_reorder(&mut b).unwrap_or(fallback) } else { fallback };
    Some(H264Sps {
        width,
        height,
        chroma_format_idc: chroma,
        bit_depth_luma_minus8: depth_luma,
        bit_depth_chroma_minus8: depth_chroma,
        max_num_reorder_frames,
    })
}

/// `max_num_reorder_frames` from an H.264 VUI (`None` without its bitstream restriction).
fn h264_vui_reorder(b: &mut Bits) -> Option<u32> {
    if b.u(1)? == 1 && b.u(8)? == 255 {
        b.skip(32)?; // aspect_ratio_idc Extended_SAR: sar_width, sar_height
    }
    if b.u(1)? == 1 {
        b.skip(1)?; // overscan_appropriate_flag
    }
    if b.u(1)? == 1 {
        b.skip(4)?; // video_format, video_full_range_flag
        if b.u(1)? == 1 {
            b.skip(24)?; // colour_primaries, transfer_characteristics, matrix_coefficients
        }
    }
    if b.u(1)? == 1 {
        b.ue()?; // chroma_sample_loc_type_top_field
        b.ue()?; // chroma_sample_loc_type_bottom_field
    }
    if b.u(1)? == 1 {
        b.skip(65)?; // num_units_in_tick, time_scale, fixed_frame_rate_flag
    }
    let mut hrd = false;
    for _ in 0..2 {
        // nal_hrd_parameters, then vcl_hrd_parameters
        if b.u(1)? == 1 {
            hrd = true;
            let cpb_count = b.ue()? + 1;
            b.skip(8)?; // bit_rate_scale, cpb_size_scale
            for _ in 0..cpb_count {
                b.ue()?;
                b.ue()?;
                b.skip(1)?;
            }
            b.skip(20)?; // four 5-bit delay/offset lengths
        }
    }
    if hrd {
        b.skip(1)?; // low_delay_hrd_flag
    }
    b.skip(1)?; // pic_struct_present_flag
    if b.u(1)? == 0 {
        return None; // no bitstream_restriction
    }
    b.skip(1)?; // motion_vectors_over_pic_boundaries_flag
    for _ in 0..4 {
        b.ue()?; // max_bytes_per_pic_denom, max_bits_per_mb_denom, log2_max_mv_length_{h,v}
    }
    b.ue()
}

/// How many pictures a decoder emitting in decode order must hold back to give display order:
/// from the H.264 or HEVC sequence parameter set in the stream's setup record (avcC/hvcC).
pub fn reorder_depth(codec: &crate::demux::Codec, config: &[u8]) -> Option<u32> {
    use crate::demux::Codec;
    match codec {
        Codec::H264 => {
            let (_, sets) = parse_avcc(config)?;
            let sps = sets.iter().find(|n| n.first().is_some_and(|b| b & 0x1F == 7))?;
            parse_h264_sps(sps).map(|s| s.max_num_reorder_frames)
        }
        Codec::Hevc => {
            let (_, sets) = parse_hvcc(config)?;
            let sps = sets.iter().find(|n| n.first().is_some_and(|b| (b >> 1) & 0x3F == 33))?;
            parse_hevc_sps(sps).map(|s| s.max_num_reorder_pics)
        }
        _ => None,
    }
}

/// Displayed picture size from an H.264 SPS NAL unit (with its header byte).
pub fn h264_sps_size(sps: &[u8]) -> Option<(u32, u32)> {
    parse_h264_sps(sps).map(|s| (s.width, s.height))
}

/// `AVCDecoderConfigurationRecord` (4-byte NAL lengths) from one SPS and one PPS.
pub fn avcc_from(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut rec = vec![1, sps.get(1).copied().unwrap_or(0), sps.get(2).copied().unwrap_or(0), sps.get(3).copied().unwrap_or(0), 0xFF, 0xE1];
    rec.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    rec.extend_from_slice(sps);
    rec.push(1);
    rec.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    rec.extend_from_slice(pps);
    // High profiles carry the chroma format and bit depths too (ISO 14496-15 5.3.3.1.2).
    if matches!(sps.get(1), Some(100 | 110 | 122 | 244))
        && let Some(s) = parse_h264_sps(sps)
    {
        rec.extend_from_slice(&[
            0xFC | s.chroma_format_idc as u8,
            0xF8 | s.bit_depth_luma_minus8 as u8,
            0xF8 | s.bit_depth_chroma_minus8 as u8,
            0,
        ]);
    }
    rec
}

/// What an HEVC SPS says about the pictures, and its general profile/tier/level bytes.
struct HevcSps {
    width: u32,
    height: u32,
    max_sub_layers_minus1: u32,
    temporal_id_nesting: u32,
    /// `general_profile_space` … `general_level_idc`: 12 bytes, as hvcC stores them.
    general_ptl: [u8; 12],
    chroma_format_idc: u32,
    bit_depth_luma_minus8: u32,
    bit_depth_chroma_minus8: u32,
    /// `sps_max_num_reorder_pics` of the highest sub-layer.
    max_num_reorder_pics: u32,
}

fn parse_hevc_sps(sps: &[u8]) -> Option<HevcSps> {
    let data = rbsp(sps.get(2..)?);
    let mut b = Bits { data: &data, pos: 0 };
    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.u(3)?;
    let temporal_id_nesting = b.u(1)?;
    let general_ptl: [u8; 12] = data.get(1..13)?.try_into().ok()?;
    b.skip(96)?;
    let mut sub_profile = [false; 8];
    let mut sub_level = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        sub_profile[i] = b.u(1)? == 1;
        sub_level[i] = b.u(1)? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        b.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if sub_profile[i] {
            b.skip(88)?;
        }
        if sub_level[i] {
            b.skip(8)?;
        }
    }
    b.ue()?; // sps_seq_parameter_set_id
    let chroma = b.ue()?;
    let separate = if chroma == 3 { b.u(1)? } else { 0 };
    let mut width = b.ue()?;
    let mut height = b.ue()?;
    if b.u(1)? == 1 {
        let (left, right, top, bottom) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let sub_w = if separate == 0 && matches!(chroma, 1 | 2) { 2 } else { 1 };
        let sub_h = if separate == 0 && chroma == 1 { 2 } else { 1 };
        width = width.checked_sub(sub_w * (left + right))?;
        height = height.checked_sub(sub_h * (top + bottom))?;
    }
    let bit_depth_luma_minus8 = b.ue()?;
    let bit_depth_chroma_minus8 = b.ue()?;
    let max_num_reorder_pics = (|| {
        b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        let all_layers = b.u(1)? == 1;
        let mut reorder = 0;
        for _ in if all_layers { 0 } else { max_sub_layers_minus1 }..=max_sub_layers_minus1 {
            b.ue()?; // sps_max_dec_pic_buffering_minus1
            reorder = b.ue()?;
            b.ue()?; // sps_max_latency_increase_plus1
        }
        Some(reorder)
    })()
    .unwrap_or(16);
    Some(HevcSps {
        width,
        height,
        max_sub_layers_minus1,
        temporal_id_nesting,
        general_ptl,
        chroma_format_idc: chroma,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        max_num_reorder_pics,
    })
}

/// Displayed picture size from an HEVC SPS NAL unit (with its 2-byte header).
pub fn hevc_sps_size(sps: &[u8]) -> Option<(u32, u32)> {
    parse_hevc_sps(sps).map(|s| (s.width, s.height))
}

/// `HEVCDecoderConfigurationRecord` (4-byte NAL lengths) from one VPS, SPS and PPS.
pub fn hvcc_from(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let info = parse_hevc_sps(sps);
    let mut rec = vec![1];
    match &info {
        Some(s) => rec.extend_from_slice(&s.general_ptl),
        None => rec.extend_from_slice(&[1, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    }
    let (chroma, luma, chroma_depth, layers, nesting) = info
        .as_ref()
        .map_or((1, 0, 0, 0, 1), |s| (s.chroma_format_idc, s.bit_depth_luma_minus8, s.bit_depth_chroma_minus8, s.max_sub_layers_minus1, s.temporal_id_nesting));
    rec.extend_from_slice(&[
        0xF0,
        0x00, // min_spatial_segmentation_idc
        0xFC, // parallelismType unknown
        0xFC | chroma as u8,
        0xF8 | luma as u8,
        0xF8 | chroma_depth as u8,
        0,
        0, // avgFrameRate
        (((layers + 1) as u8) << 3) | ((nesting as u8) << 2) | 3,
        3,
    ]);
    for (nal_type, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        rec.push(0x80 | nal_type);
        rec.extend_from_slice(&1u16.to_be_bytes());
        rec.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        rec.extend_from_slice(nal);
    }
    rec
}

#[cfg(test)]
mod tests {

    /// The video stream of a fixture and its setup record, through the player's demuxers.
    #[cfg(feature = "native")]
    fn video_config(name: &str) -> (crate::demux::Codec, Vec<u8>) {
        let source = crate::Source::parse(&format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut src = source.open().unwrap();
        let format = crate::demux::probe(src.as_mut()).unwrap().unwrap();
        let d = crate::backend::Registry::empty_with_native().open_demuxer(&source, format, src, None).unwrap();
        let s = d.streams().iter().find(|s| s.kind == crate::demux::StreamKind::Video).unwrap();
        (s.codec.clone(), s.extradata.clone().unwrap())
    }

    #[cfg(feature = "native")]
    #[test]
    fn reorder_depth_matches_ffprobes_has_b_frames() {
        for (name, depth) in [("h264_aac.mp4", 0), ("h264_aac.ts", 2), ("hevc.mkv", 2), ("hevc_10bit.mp4", 2)] {
            let (codec, config) = video_config(name);
            assert_eq!(reorder_depth(&codec, &config), Some(depth), "{name}");
        }
        assert_eq!(reorder_depth(&crate::demux::Codec::H264, &[1, 2, 3]), None);
    }
    use super::*;

    /// avcC with one SPS [0x67, 1, 2] and one PPS [0x68, 3], `length_size` byte prefixes.
    fn avcc(length_size: u8) -> Vec<u8> {
        vec![1, 0x64, 0, 0x1F, 0xFC | (length_size - 1), 0xE1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 3]
    }

    #[test]
    fn keyframes_get_parameter_sets_and_every_nal_a_start_code() {
        for size in [1u8, 2, 4] {
            let conv = AnnexB::from_config(ParamSetFormat::Avcc, &avcc(size)).unwrap();
            let mut pkt = Vec::new();
            for nal in [&[0x65u8, 9, 9][..], &[0x06, 7]] {
                let len = nal.len() as u32;
                pkt.extend_from_slice(&len.to_be_bytes()[4 - size as usize..]);
                pkt.extend_from_slice(nal);
            }
            let mut out = Vec::new();
            conv.convert(&pkt, true, &mut out).unwrap();
            assert_eq!(out, [&[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3][..], &[0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0, 1, 6, 7]].concat(), "size {size}");
            out.clear();
            conv.convert(&pkt, false, &mut out).unwrap();
            assert_eq!(out, [0, 0, 0, 1, 0x65, 9, 9, 0, 0, 0, 1, 6, 7], "non-keyframes carry no parameter sets");
        }
    }

    #[test]
    fn truncated_packets_and_configs_are_errors() {
        let conv = AnnexB::from_config(ParamSetFormat::Avcc, &avcc(4)).unwrap();
        let mut out = Vec::new();
        assert!(conv.convert(&[0, 0, 0, 9, 1, 2], false, &mut out).is_err(), "length past the end");
        assert!(conv.convert(&[0, 0], false, &mut out).is_err(), "prefix cut short");
        assert!(AnnexB::from_config(ParamSetFormat::Avcc, &avcc(4)[..9]).is_none());
        assert!(AnnexB::from_config(ParamSetFormat::Avcc, &[0; 4]).is_none());
        assert!(AnnexB::from_config(ParamSetFormat::Hvcc, &[1; 10]).is_none());
    }

    /// x264 (High 4:4:4 Predictive, 320x240): SPS, PPS.
    const H264_444: &[&[u8]] = &[&[0x67, 0xf4, 0x00, 0x0d, 0x91, 0x9b, 0x28, 0x28, 0x3f, 0x60, 0x22, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x64, 0x1e, 0x28, 0x53, 0x2c], &[0x68, 0xeb, 0xe3, 0xc4, 0x48, 0x44]];
    /// x264 (High 4:2:0, 318x238: cropped from 320x240).
    const H264_CROP: &[&[u8]] = &[&[0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfe, 0xab, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x42, 0x99, 0x60], &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]];
    /// x265 (Main 4:4:4 / RExt, 352x288): VPS, SPS, PPS.
    const HEVC: &[&[u8]] = &[&[0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00, 0x00, 0x03, 0x00, 0x00, 0x3c, 0x95, 0x98, 0x09], &[0x42, 0x01, 0x01, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00, 0x00, 0x03, 0x00, 0x00, 0x3c, 0x90, 0x01, 0x61, 0x00, 0x90, 0xb2, 0xca, 0xcd, 0x24, 0x99, 0x5e, 0x02, 0xdc, 0x08, 0x08, 0x00, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x01, 0x90, 0x80], &[0x44, 0x01, 0xc1, 0x72, 0x86, 0x0c, 0x46, 0x24]];

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let es = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 3, 0, 0, 1, 0x65, 4, 0];
        assert_eq!(split_annex_b(&es), [&[0x67, 1][..], &[0x68, 2, 3], &[0x65, 4]], "trailing zero bytes are not data");
        assert!(split_annex_b(&[]).is_empty());
        assert_eq!(split_annex_b(&[9, 9, 0, 0, 1, 0x41]), [&[0x41][..]], "bytes before the first start code are dropped");
    }

    #[test]
    fn avcc_round_trips_through_annex_b() {
        for sets in [H264_444, H264_CROP] {
            let rec = avcc_from(sets[0], sets[1]);
            assert_eq!((rec[0], rec[1], rec[3]), (1, sets[0][1], sets[0][3]), "version, profile, level");
            let conv = AnnexB::from_config(ParamSetFormat::Avcc, &rec).unwrap();
            let mut out = Vec::new();
            conv.convert(&[], true, &mut out).unwrap();
            assert_eq!(out, annex_b(sets));
            assert_eq!(conv.length_size, 4);
        }
    }

    #[test]
    fn h264_picture_size_with_and_without_cropping() {
        assert_eq!(h264_sps_size(H264_444[0]), Some((320, 240)));
        assert_eq!(h264_sps_size(H264_CROP[0]), Some((318, 238)));
        assert_eq!(h264_sps_size(&[0x67, 0x64]), None, "truncated");
    }

    #[test]
    fn hvcc_round_trips_and_carries_profile() {
        let rec = hvcc_from(HEVC[0], HEVC[1], HEVC[2]);
        assert_eq!(rec[0], 1);
        assert_eq!(rec[1] & 0x1F, 4, "general_profile_idc copied from the SPS (RExt)");
        assert_eq!(rec[12], 0x3c, "general_level_idc (level 2)");
        assert_eq!(rec[16] & 3, 3, "chroma_format_idc 4:4:4");
        let conv = AnnexB::from_config(ParamSetFormat::Hvcc, &rec).unwrap();
        let mut out = Vec::new();
        conv.convert(&[], true, &mut out).unwrap();
        assert_eq!(out, annex_b(HEVC));
        assert_eq!(hevc_sps_size(HEVC[1]), Some((352, 288)));
    }
}
