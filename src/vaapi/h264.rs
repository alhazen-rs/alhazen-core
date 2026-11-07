//! What VA-API leaves to the application for H.264: reading the parameter sets and slice
//! headers, picture order counts, which pictures are kept for reference, the reference lists
//! of every slice, and the order pictures come out in. (NVDEC's parser does all of this
//! itself; VA-API only decodes slices.)
//!
//! Progressive pictures (frames, MBAFF included); field pictures and slice groups (FMO) are
//! refused, and the next backend takes the stream. Section numbers are H.264 (08/2021).

use std::time::Duration;

use crate::{Error, Result};

fn bad(what: &str) -> Error {
    Error::Decode(format!("H.264: {what}"))
}

// ---- Bits ------------------------------------------------------------------------------

/// The payload of a NAL unit without its emulation prevention bytes.
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

pub struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, at: 0 }
    }
    pub fn position(&self) -> usize {
        self.at
    }
    pub fn bit(&mut self) -> Result<bool> {
        let byte = *self.data.get(self.at / 8).ok_or_else(|| bad("header runs past its end"))?;
        self.at += 1;
        Ok(byte >> (7 - (self.at - 1) % 8) & 1 == 1)
    }
    pub fn flag(&mut self) -> Result<bool> {
        self.bit()
    }
    pub fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = v << 1 | u32::from(self.bit()?);
        }
        Ok(v)
    }
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(bad("bad exp-Golomb code"));
            }
        }
        let rest = self.bits(zeros)?;
        Ok(((1u64 << zeros) - 1 + u64::from(rest)).min(u64::from(u32::MAX)) as u32)
    }
    pub fn se(&mut self) -> Result<i32> {
        let k = i64::from(self.ue()?);
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
    /// Whether anything but the trailing bits is left (7.2).
    fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|b| *b != 0) else { return false };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.at < stop
    }
}

// ---- Parameter sets ---------------------------------------------------------------------

/// Table 7-3 and 7-4, in zig-zag scan order (as the bitstream has them, and as VA-API wants).
const DEFAULT_4X4_INTRA: [u8; 16] = [6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42];
const DEFAULT_4X4_INTER: [u8; 16] = [10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34];
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31, 31, 33, 33, 33, 33, 33,
    36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 28, 28, 28, 28, 28,
    30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

/// Scaling lists: six 4x4 (Y, Cb, Cr intra; Y, Cb, Cr inter), six 8x8 (the same order;
/// only the first two exist outside 4:4:4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scaling {
    pub x4: [[u8; 16]; 6],
    pub x8: [[u8; 64]; 6],
}

impl Scaling {
    pub fn flat() -> Scaling {
        Scaling { x4: [[16; 16]; 6], x8: [[16; 64]; 6] }
    }
}

/// One scaling list (7.3.2.1.1.1); whether the default one is meant instead.
fn scaling_list(bits: &mut Bits, list: &mut [u8]) -> Result<bool> {
    let (mut last, mut next) = (8i32, 8i32);
    let mut use_default = false;
    for (j, entry) in list.iter_mut().enumerate() {
        if next != 0 {
            let delta = bits.se()?;
            next = (last + delta + 256) % 256;
            use_default = j == 0 && next == 0;
        }
        *entry = if next == 0 { last } else { next } as u8;
        last = i32::from(*entry);
    }
    Ok(use_default)
}

/// Reads `count` lists into `out`, each missing one by its fallback (Table 7-2): rule A
/// (`fallback` = None) falls to the defaults, rule B to `fallback`'s lists.
fn scaling_lists(bits: &mut Bits, count: usize, fallback: Option<&Scaling>) -> Result<Scaling> {
    let mut out = Scaling::flat();
    for i in 0..count {
        let present = bits.flag()?;
        if i < 6 {
            let default = if i < 3 { DEFAULT_4X4_INTRA } else { DEFAULT_4X4_INTER };
            let missing = match (i, fallback) {
                (0 | 3, None) => default,
                (0 | 3, Some(f)) => f.x4[i],
                _ => out.x4[i - 1],
            };
            if present {
                let mut list = [0u8; 16];
                out.x4[i] = if scaling_list(bits, &mut list)? { default } else { list };
            } else {
                out.x4[i] = missing;
            }
        } else {
            let j = i - 6;
            let default = if j % 2 == 0 { DEFAULT_8X8_INTRA } else { DEFAULT_8X8_INTER };
            let missing = match (j, fallback) {
                (0 | 1, None) => default,
                (0 | 1, Some(f)) => f.x8[j],
                _ => out.x8[j - 2],
            };
            if present {
                let mut list = [0u8; 64];
                out.x8[j] = if scaling_list(bits, &mut list)? { default } else { list };
            } else {
                out.x8[j] = missing;
            }
        }
    }
    // 8x8 lists past those sent (fewer than six outside 4:4:4) follow their own fallbacks.
    for i in count.max(6)..12 {
        let j = i - 6;
        out.x8[j] = match (j, fallback) {
            (0, None) => DEFAULT_8X8_INTRA,
            (1, None) => DEFAULT_8X8_INTER,
            (0 | 1, Some(f)) => f.x8[j],
            _ => out.x8[j - 2],
        };
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Sps {
    pub id: u32,
    pub profile_idc: u32,
    pub constraint_flags: u32,
    pub level_idc: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub qpprime_y_zero_transform_bypass: bool,
    /// The SPS's own scaling lists, if it has any (else flat).
    pub scaling: Option<Scaling>,
    pub log2_max_frame_num: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub width_mbs: u32,
    pub height_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    /// Left, right, top, bottom, in luma samples.
    pub crop: [u32; 4],
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
    /// The VUI's colour: full range, and the H.273 matrix if it says.
    pub full_range: bool,
    pub matrix_coefficients: Option<u8>,
}

impl Sps {
    pub fn height_mbs(&self) -> u32 {
        self.height_map_units * if self.frame_mbs_only { 1 } else { 2 }
    }
    pub fn coded_size(&self) -> (u32, u32) {
        (self.width_mbs * 16, self.height_mbs() * 16)
    }
    /// The picture as shown: the coded size less the cropping.
    pub fn display_rect(&self) -> (u32, u32, u32, u32) {
        let (w, h) = self.coded_size();
        let [l, r, t, b] = self.crop;
        (l, t, w.saturating_sub(l + r), h.saturating_sub(t + b))
    }
    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
    /// Frames the decoder must hold (A.3.1, or the VUI's own figure), at least the references.
    pub fn dpb_frames(&self) -> usize {
        let from_level = match self.level_idc {
            9 | 10 => 396,
            11 => if self.constraint_flags & 0x10 != 0 && self.profile_idc == 66 { 396 } else { 900 },
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            51 | 52 => 184320,
            _ => 696320,
        };
        let frame_mbs = (self.width_mbs * self.height_mbs()).max(1);
        let by_level = (from_level / frame_mbs).clamp(1, 16) as usize;
        let wanted = self.max_dec_frame_buffering.map_or(by_level, |n| n as usize);
        wanted.max(self.max_num_ref_frames as usize).clamp(1, 16)
    }
    /// How many pictures may come before the next one shown.
    pub fn reorder_depth(&self) -> usize {
        if let Some(n) = self.max_num_reorder_frames {
            return (n as usize).min(self.dpb_frames());
        }
        // Baseline has no B-frames; anything else may reorder up to the DPB.
        if self.profile_idc == 66 { 0 } else { self.dpb_frames() }
    }
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut b = Bits::new(rbsp);
    let profile_idc = b.bits(8)?;
    let constraint_flags = b.bits(8)?;
    let level_idc = b.bits(8)?;
    let id = b.ue()?;
    if id > 31 {
        return Err(bad("SPS id out of range"));
    }
    let (mut chroma_format_idc, mut separate_colour_plane, mut bit_depth_luma, mut bit_depth_chroma, mut bypass, mut scaling) = (1, false, 8, 8, false, None);
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = b.flag()?;
        }
        bit_depth_luma = b.ue()? + 8;
        bit_depth_chroma = b.ue()? + 8;
        bypass = b.flag()?;
        if b.flag()? {
            scaling = Some(scaling_lists(&mut b, if chroma_format_idc == 3 { 12 } else { 8 }, None)?);
        }
    }
    let log2_max_frame_num = b.ue()? + 4;
    let pic_order_cnt_type = b.ue()?;
    let (mut log2_max_poc_lsb, mut delta_pic_order_always_zero, mut offset_for_non_ref_pic, mut offset_for_top_to_bottom_field, mut offset_for_ref_frame) = (0, false, 0, 0, Vec::new());
    match pic_order_cnt_type {
        0 => log2_max_poc_lsb = b.ue()? + 4,
        1 => {
            delta_pic_order_always_zero = b.flag()?;
            offset_for_non_ref_pic = b.se()?;
            offset_for_top_to_bottom_field = b.se()?;
            let n = b.ue()?;
            if n > 255 {
                return Err(bad("POC cycle too long"));
            }
            for _ in 0..n {
                offset_for_ref_frame.push(b.se()?);
            }
        }
        2 => {}
        _ => return Err(bad("unknown POC type")),
    }
    let max_num_ref_frames = b.ue()?;
    let gaps_in_frame_num_allowed = b.flag()?;
    let width_mbs = b.ue()? + 1;
    let height_map_units = b.ue()? + 1;
    let frame_mbs_only = b.flag()?;
    let mb_adaptive_frame_field = if frame_mbs_only { false } else { b.flag()? };
    let direct_8x8_inference = b.flag()?;
    let mut crop = [0; 4];
    if b.flag()? {
        // Crop units (7-19, 7-20): chroma subsampling across, and two for interlaced rows.
        let (unit_x, unit_y) = match (chroma_format_idc, separate_colour_plane) {
            (0, _) | (3, true) => (1, if frame_mbs_only { 1 } else { 2 }),
            (1, false) => (2, 2 * if frame_mbs_only { 1 } else { 2 }),
            (2, false) => (2, if frame_mbs_only { 1 } else { 2 }),
            _ => (1, if frame_mbs_only { 1 } else { 2 }),
        };
        crop = [b.ue()? * unit_x, b.ue()? * unit_x, b.ue()? * unit_y, b.ue()? * unit_y];
    }
    let (mut max_num_reorder_frames, mut max_dec_frame_buffering, mut full_range, mut matrix_coefficients) = (None, None, false, None);
    if b.flag()? {
        // VUI (E.1.1): its colour and its bitstream restriction.
        if b.flag()? && b.bits(8)? == 255 {
            b.bits(32)?;
        }
        if b.flag()? {
            b.flag()?;
        }
        if b.flag()? {
            b.bits(3)?;
            full_range = b.flag()?;
            if b.flag()? {
                b.bits(16)?;
                matrix_coefficients = Some(b.bits(8)? as u8);
            }
        }
        if b.flag()? {
            b.ue()?;
            b.ue()?;
        }
        if b.flag()? {
            b.bits(32)?;
            b.bits(32)?;
            b.flag()?;
        }
        let nal_hrd = b.flag()?;
        if nal_hrd {
            skip_hrd(&mut b)?;
        }
        let vcl_hrd = b.flag()?;
        if vcl_hrd {
            skip_hrd(&mut b)?;
        }
        if nal_hrd || vcl_hrd {
            b.flag()?;
        }
        b.flag()?;
        // A VUI cut short (some encoders) just ends here.
        if let Ok(true) = b.flag() {
            let restriction = (|| -> Result<(u32, u32)> {
                b.flag()?;
                b.ue()?;
                b.ue()?;
                b.ue()?;
                b.ue()?;
                Ok((b.ue()?, b.ue()?))
            })();
            if let Ok((reorder, buffering)) = restriction {
                max_num_reorder_frames = Some(reorder);
                max_dec_frame_buffering = Some(buffering);
            }
        }
    }
    Ok(Sps {
        id,
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        separate_colour_plane,
        bit_depth_luma,
        bit_depth_chroma,
        qpprime_y_zero_transform_bypass: bypass,
        scaling,
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        width_mbs,
        height_map_units,
        frame_mbs_only,
        mb_adaptive_frame_field,
        direct_8x8_inference,
        crop,
        max_num_reorder_frames,
        max_dec_frame_buffering,
        full_range,
        matrix_coefficients,
    })
}

fn skip_hrd(b: &mut Bits) -> Result<()> {
    let count = b.ue()?;
    if count > 31 {
        return Err(bad("bad HRD"));
    }
    b.bits(8)?;
    for _ in 0..=count {
        b.ue()?;
        b.ue()?;
        b.flag()?;
    }
    b.bits(20)?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    /// The PPS's own scaling lists, if any (else the SPS's).
    pub scaling: Option<Scaling>,
    pub second_chroma_qp_index_offset: i32,
}

impl Pps {
    /// The scaling lists in force for a picture using this PPS and `sps`.
    pub fn scaling(&self, sps: &Sps) -> Scaling {
        self.scaling.clone().or_else(|| sps.scaling.clone()).unwrap_or_else(Scaling::flat)
    }
}

/// A PPS; `sps` is the SPS sets it may name (its scaling lists are needed for the fallbacks).
pub fn parse_pps(rbsp: &[u8], sps: &[Option<Sps>]) -> Result<Pps> {
    let mut b = Bits::new(rbsp);
    let id = b.ue()?;
    let sps_id = b.ue()?;
    if id > 255 || sps_id > 31 {
        return Err(bad("PPS id out of range"));
    }
    let entropy_coding_mode = b.flag()?;
    let bottom_field_pic_order_in_frame_present = b.flag()?;
    if b.ue()? > 0 {
        return Err(Error::Unsupported("H.264 slice groups (FMO) through VA-API"));
    }
    let num_ref_idx_l0_default_active = b.ue()? + 1;
    let num_ref_idx_l1_default_active = b.ue()? + 1;
    let weighted_pred = b.flag()?;
    let weighted_bipred_idc = b.bits(2)?;
    let pic_init_qp_minus26 = b.se()?;
    let pic_init_qs_minus26 = b.se()?;
    let chroma_qp_index_offset = b.se()?;
    let deblocking_filter_control_present = b.flag()?;
    let constrained_intra_pred = b.flag()?;
    let redundant_pic_cnt_present = b.flag()?;
    let (mut transform_8x8_mode, mut scaling, mut second_chroma_qp_index_offset) = (false, None, chroma_qp_index_offset);
    if b.more_rbsp_data() {
        transform_8x8_mode = b.flag()?;
        if b.flag()? {
            let sps = sps.get(sps_id as usize).and_then(Option::as_ref).ok_or_else(|| bad("PPS names an SPS not seen"))?;
            let count = 6 + if transform_8x8_mode { if sps.chroma_format_idc == 3 { 6 } else { 2 } } else { 0 };
            // Rule B falls back to the SPS's lists when it has them, rule A otherwise.
            scaling = Some(scaling_lists(&mut b, count, sps.scaling.as_ref())?);
        }
        second_chroma_qp_index_offset = b.se()?;
    }
    Ok(Pps {
        id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_l0_default_active,
        num_ref_idx_l1_default_active,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
        scaling,
        second_chroma_qp_index_offset,
    })
}

// ---- Slice headers ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    fn of(n: u32) -> SliceType {
        match n % 5 {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::Sp,
            _ => SliceType::Si,
        }
    }
    /// VA-API's number for it (the standard's, 0..=4).
    pub fn number(self) -> u8 {
        match self {
            SliceType::P => 0,
            SliceType::B => 1,
            SliceType::I => 2,
            SliceType::Sp => 3,
            SliceType::Si => 4,
        }
    }
    fn is_p(self) -> bool {
        matches!(self, SliceType::P | SliceType::Sp)
    }
}

/// One reference list modification (7.3.3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modification {
    pub idc: u32,
    pub value: u32,
}

/// A memory management control operation (7.3.3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mmco {
    pub op: u32,
    pub difference_of_pic_nums_minus1: u32,
    pub long_term_pic_num: u32,
    pub long_term_frame_idx: u32,
    pub max_long_term_frame_idx_plus1: u32,
}

/// Explicit weights for one reference (7.3.3.2): luma (weight, offset), chroma per component.
#[derive(Clone, Copy, Debug, Default)]
pub struct Weight {
    pub luma: Option<(i32, i32)>,
    pub chroma: Option<[(i32, i32); 2]>,
}

#[derive(Clone, Debug, Default)]
pub struct Weights {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    pub l0: Vec<Weight>,
    pub l1: Vec<Weight>,
}

#[derive(Clone, Debug)]
pub struct SliceHeader {
    pub nal_ref_idc: u32,
    pub idr: bool,
    pub first_mb: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub mbaff: bool,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_l0_active: u32,
    pub num_ref_idx_l1_active: u32,
    pub mods_l0: Vec<Modification>,
    pub mods_l1: Vec<Modification>,
    pub weights: Option<Weights>,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    /// The operations, or `None` for the sliding window.
    pub mmcos: Option<Vec<Mmco>>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    /// Where the slice data starts, in bits from the NAL header's first bit, emulation
    /// prevention bytes not counted: what VA-API drivers take (as FFmpeg and GStreamer pass).
    pub data_bit_offset: u32,
}

impl SliceHeader {
    pub fn has_mmco5(&self) -> bool {
        self.mmcos.as_ref().is_some_and(|ops| ops.iter().any(|m| m.op == 5))
    }
}

fn modifications(b: &mut Bits) -> Result<Vec<Modification>> {
    let mut out = Vec::new();
    if b.flag()? {
        loop {
            let idc = b.ue()?;
            match idc {
                0..=2 => out.push(Modification { idc, value: b.ue()? }),
                3 => break,
                _ => return Err(bad("bad reference list modification")),
            }
            if out.len() > 64 {
                return Err(bad("too many reference list modifications"));
            }
        }
    }
    Ok(out)
}

fn weight_list(b: &mut Bits, count: u32, chroma: bool) -> Result<Vec<Weight>> {
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut w = Weight::default();
        if b.flag()? {
            w.luma = Some((b.se()?, b.se()?));
        }
        if chroma && b.flag()? {
            w.chroma = Some([(b.se()?, b.se()?), (b.se()?, b.se()?)]);
        }
        out.push(w);
    }
    Ok(out)
}

/// A slice header (7.3.3). `nal` is the whole unescaped NAL unit, header byte first.
pub fn parse_slice_header(nal: &[u8], sps_sets: &[Option<Sps>], pps_sets: &[Option<Pps>]) -> Result<SliceHeader> {
    let header = *nal.first().ok_or_else(|| bad("empty NAL unit"))?;
    let (nal_ref_idc, nal_type) = (u32::from(header >> 5 & 3), header & 0x1f);
    let mut b = Bits::new(nal);
    b.bits(8)?;
    let first_mb = b.ue()?;
    let slice_type = SliceType::of(b.ue()?);
    let pps_id = b.ue()?;
    let pps = pps_sets.get(pps_id as usize).and_then(Option::as_ref).ok_or_else(|| bad("slice names a PPS not seen"))?;
    let sps = sps_sets.get(pps.sps_id as usize).and_then(Option::as_ref).ok_or_else(|| bad("PPS names an SPS not seen"))?;
    if sps.separate_colour_plane {
        b.bits(2)?;
    }
    let frame_num = b.bits(sps.log2_max_frame_num)?;
    let mut mbaff = false;
    if !sps.frame_mbs_only {
        if b.flag()? {
            return Err(Error::Unsupported("H.264 field pictures through VA-API"));
        }
        mbaff = sps.mb_adaptive_frame_field;
    }
    let idr = nal_type == 5;
    let idr_pic_id = if idr { b.ue()? } else { 0 };
    let (mut poc_lsb, mut delta_poc_bottom, mut delta_poc) = (0, 0, [0, 0]);
    if sps.pic_order_cnt_type == 0 {
        poc_lsb = b.bits(sps.log2_max_poc_lsb)?;
        if pps.bottom_field_pic_order_in_frame_present {
            delta_poc_bottom = b.se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        delta_poc[0] = b.se()?;
        if pps.bottom_field_pic_order_in_frame_present {
            delta_poc[1] = b.se()?;
        }
    }
    let redundant_pic_cnt = if pps.redundant_pic_cnt_present { b.ue()? } else { 0 };
    let direct_spatial_mv_pred = slice_type == SliceType::B && b.flag()?;
    let (mut l0, mut l1) = (pps.num_ref_idx_l0_default_active, pps.num_ref_idx_l1_default_active);
    if (slice_type.is_p() || slice_type == SliceType::B) && b.flag()? {
        l0 = b.ue()? + 1;
        if slice_type == SliceType::B {
            l1 = b.ue()? + 1;
        }
    }
    if l0 > 32 || l1 > 32 {
        return Err(bad("too many references"));
    }
    if slice_type == SliceType::I || slice_type == SliceType::Si {
        (l0, l1) = (0, 0);
    } else if slice_type.is_p() {
        l1 = 0;
    }
    let mods_l0 = if slice_type != SliceType::I && slice_type != SliceType::Si { modifications(&mut b)? } else { Vec::new() };
    let mods_l1 = if slice_type == SliceType::B { modifications(&mut b)? } else { Vec::new() };
    let chroma = sps.chroma_format_idc != 0 && !sps.separate_colour_plane;
    let weights = if (pps.weighted_pred && slice_type.is_p()) || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B) {
        let luma_log2_denom = b.ue()?;
        let chroma_log2_denom = if chroma { b.ue()? } else { 0 };
        let wl0 = weight_list(&mut b, l0, chroma)?;
        let wl1 = if slice_type == SliceType::B { weight_list(&mut b, l1, chroma)? } else { Vec::new() };
        Some(Weights { luma_log2_denom, chroma_log2_denom, l0: wl0, l1: wl1 })
    } else {
        None
    };
    let (mut no_output_of_prior_pics, mut long_term_reference, mut mmcos) = (false, false, None);
    if nal_ref_idc != 0 {
        if idr {
            no_output_of_prior_pics = b.flag()?;
            long_term_reference = b.flag()?;
        } else if b.flag()? {
            let mut ops = Vec::new();
            loop {
                let op = b.ue()?;
                if op == 0 {
                    break;
                }
                if op > 6 || ops.len() > 66 {
                    return Err(bad("bad memory management operation"));
                }
                let mut m = Mmco { op, ..Default::default() };
                if op == 1 || op == 3 {
                    m.difference_of_pic_nums_minus1 = b.ue()?;
                }
                if op == 2 {
                    m.long_term_pic_num = b.ue()?;
                }
                if op == 3 || op == 6 {
                    m.long_term_frame_idx = b.ue()?;
                }
                if op == 4 {
                    m.max_long_term_frame_idx_plus1 = b.ue()?;
                }
                ops.push(m);
            }
            mmcos = Some(ops);
        }
    }
    let cabac_init_idc = if pps.entropy_coding_mode && slice_type != SliceType::I && slice_type != SliceType::Si { b.ue()? } else { 0 };
    let slice_qp_delta = b.se()?;
    if slice_type == SliceType::Sp || slice_type == SliceType::Si {
        if slice_type == SliceType::Sp {
            b.flag()?;
        }
        b.se()?;
    }
    let (mut disable_deblocking_filter_idc, mut slice_alpha_c0_offset_div2, mut slice_beta_offset_div2) = (0, 0, 0);
    if pps.deblocking_filter_control_present {
        disable_deblocking_filter_idc = b.ue()?;
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset_div2 = b.se()?;
            slice_beta_offset_div2 = b.se()?;
        }
    }
    Ok(SliceHeader {
        nal_ref_idc,
        idr,
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        mbaff,
        idr_pic_id,
        poc_lsb,
        delta_poc_bottom,
        delta_poc,
        redundant_pic_cnt,
        direct_spatial_mv_pred,
        num_ref_idx_l0_active: l0,
        num_ref_idx_l1_active: l1,
        mods_l0,
        mods_l1,
        weights,
        no_output_of_prior_pics,
        long_term_reference,
        mmcos,
        cabac_init_idc,
        slice_qp_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        data_bit_offset: b.position() as u32,
    })
}

// ---- Picture order (8.2.1) --------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct PocState {
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
}

impl PocState {
    /// The picture's (top, bottom) order counts. Call for every picture, in decoding order;
    /// then [`PocState::after`] once it's decoded.
    pub fn compute(&self, sps: &Sps, sh: &SliceHeader) -> (i32, i32) {
        match sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let (prev_msb, prev_lsb) = if sh.idr { (0, 0) } else { (self.prev_poc_msb, self.prev_poc_lsb) };
                let lsb = sh.poc_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                (top, top + sh.delta_poc_bottom)
            }
            1 => {
                let offset = self.frame_num_offset(sps, sh);
                let cycle = sps.offset_for_ref_frame.len() as i32;
                let mut abs = if cycle != 0 { offset + sh.frame_num as i32 } else { 0 };
                if sh.nal_ref_idc == 0 && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0;
                if abs > 0 {
                    let (count, in_cycle) = ((abs - 1) / cycle, (abs - 1) % cycle);
                    let delta: i32 = sps.offset_for_ref_frame.iter().sum();
                    expected = count * delta + sps.offset_for_ref_frame[..=in_cycle as usize].iter().sum::<i32>();
                }
                if sh.nal_ref_idc == 0 {
                    expected += sps.offset_for_non_ref_pic;
                }
                let top = expected + sh.delta_poc[0];
                (top, top + sps.offset_for_top_to_bottom_field + sh.delta_poc[1])
            }
            _ => {
                let offset = self.frame_num_offset(sps, sh);
                let poc = if sh.idr {
                    0
                } else if sh.nal_ref_idc == 0 {
                    2 * (offset + sh.frame_num as i32) - 1
                } else {
                    2 * (offset + sh.frame_num as i32)
                };
                (poc, poc)
            }
        }
    }

    fn frame_num_offset(&self, sps: &Sps, sh: &SliceHeader) -> i32 {
        if sh.idr {
            0
        } else if self.prev_frame_num > sh.frame_num {
            self.prev_frame_num_offset + sps.max_frame_num() as i32
        } else {
            self.prev_frame_num_offset
        }
    }

    /// Carries what the next picture's count depends on. `poc` is this one's (top, bottom),
    /// already reset to zero-based if it had an MMCO 5.
    pub fn after(&mut self, sps: &Sps, sh: &SliceHeader, poc: (i32, i32)) {
        let mmco5 = sh.has_mmco5();
        if sps.pic_order_cnt_type == 0 && sh.nal_ref_idc != 0 {
            if mmco5 {
                (self.prev_poc_msb, self.prev_poc_lsb) = (0, poc.0);
            } else {
                let lsb = sh.poc_lsb as i32;
                (self.prev_poc_msb, self.prev_poc_lsb) = (poc.0 - lsb, lsb);
            }
        }
        if sps.pic_order_cnt_type != 0 {
            self.prev_frame_num_offset = if mmco5 { 0 } else { self.frame_num_offset(sps, sh) };
        }
        self.prev_frame_num = if mmco5 { 0 } else { sh.frame_num };
    }

    /// A frame number skipped (a gap): counted as if a picture had been there.
    fn gap(&mut self, sps: &Sps, frame_num: u32) {
        if sps.pic_order_cnt_type != 0 {
            let offset = if self.prev_frame_num > frame_num { self.prev_frame_num_offset + sps.max_frame_num() as i32 } else { self.prev_frame_num_offset };
            self.prev_frame_num_offset = offset;
        }
        self.prev_frame_num = frame_num;
    }
}

// ---- Decoded pictures (8.2.4, 8.2.5, C.4) -----------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference {
    None,
    Short,
    Long,
}

/// A decoded picture in the buffer. `surface` is where its pixels are (VA's surface id).
#[derive(Clone, Copy, Debug)]
pub struct Picture {
    pub surface: u32,
    pub frame_num: u32,
    pub top_poc: i32,
    pub bottom_poc: i32,
    pub reference: Reference,
    pub long_term_frame_idx: u32,
    pub output_needed: bool,
    /// A frame number filled in for a gap: referenced but never decoded or shown.
    pub non_existing: bool,
    pub pts: Duration,
}

impl Picture {
    pub fn poc(&self) -> i32 {
        self.top_poc.min(self.bottom_poc)
    }
}

/// A reference list entry: the picture's index in the buffer.
pub type ListEntry = Option<usize>;

#[derive(Default)]
pub struct Dpb {
    pub pictures: Vec<Picture>,
    max_long_term_frame_idx: Option<u32>,
    pub poc: PocState,
    prev_ref_frame_num: u32,
}

impl Dpb {
    pub fn clear(&mut self) {
        *self = Dpb::default();
    }

    /// Each short-term reference's PicNum (8.2.4.1): its frame number, wrapped below the
    /// current one.
    fn pic_num(&self, sps: &Sps, frame_num: u32, current: u32) -> i32 {
        if frame_num > current { frame_num as i32 - sps.max_frame_num() as i32 } else { frame_num as i32 }
    }

    /// Fills a gap in frame numbers (8.2.5.2) with non-existing references, before `sh`.
    /// `surface` stands in for them (they're never decoded from).
    pub fn fill_gap(&mut self, sps: &Sps, sh: &SliceHeader, surface: u32) {
        if sh.idr || sh.frame_num == self.prev_ref_frame_num || sh.frame_num == (self.prev_ref_frame_num + 1) % sps.max_frame_num() {
            return;
        }
        let mut frame_num = (self.prev_ref_frame_num + 1) % sps.max_frame_num();
        let mut filled = 0;
        while frame_num != sh.frame_num && filled < 32 {
            self.sliding_window(sps, frame_num);
            self.pictures.push(Picture { surface, frame_num, top_poc: 0, bottom_poc: 0, reference: Reference::Short, long_term_frame_idx: 0, output_needed: false, non_existing: true, pts: Duration::ZERO });
            self.poc.gap(sps, frame_num);
            self.prev_ref_frame_num = frame_num;
            frame_num = (frame_num + 1) % sps.max_frame_num();
            filled += 1;
        }
    }

    /// Reference lists 0 and 1 for a slice of the current picture (8.2.4.2, 8.2.4.3).
    pub fn ref_lists(&self, sps: &Sps, sh: &SliceHeader, current_poc: i32) -> (Vec<ListEntry>, Vec<ListEntry>) {
        let short: Vec<usize> = (0..self.pictures.len()).filter(|i| self.pictures[*i].reference == Reference::Short).collect();
        let mut long: Vec<usize> = (0..self.pictures.len()).filter(|i| self.pictures[*i].reference == Reference::Long).collect();
        long.sort_by_key(|i| self.pictures[*i].long_term_frame_idx);
        let pic_num = |i: usize| self.pic_num(sps, self.pictures[i].frame_num, sh.frame_num);
        let poc = |i: usize| self.pictures[i].poc();
        let (mut l0, mut l1): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        if sh.slice_type.is_p() {
            let mut s = short.clone();
            s.sort_by_key(|i| std::cmp::Reverse(pic_num(*i)));
            l0 = s.into_iter().chain(long.iter().copied()).collect();
        } else if sh.slice_type == SliceType::B {
            let mut before: Vec<usize> = short.iter().copied().filter(|i| poc(*i) < current_poc).collect();
            let mut after: Vec<usize> = short.iter().copied().filter(|i| poc(*i) > current_poc).collect();
            before.sort_by_key(|i| std::cmp::Reverse(poc(*i)));
            after.sort_by_key(|i| poc(*i));
            l0 = before.iter().chain(after.iter()).chain(long.iter()).copied().collect();
            l1 = after.iter().chain(before.iter()).chain(long.iter()).copied().collect();
            if l1.len() > 1 && l1 == l0 {
                l1.swap(0, 1);
            }
        }
        let l0 = self.modify(sps, sh, l0, &sh.mods_l0, sh.num_ref_idx_l0_active);
        let l1 = self.modify(sps, sh, l1, &sh.mods_l1, sh.num_ref_idx_l1_active);
        (l0, l1)
    }

    fn modify(&self, sps: &Sps, sh: &SliceHeader, initial: Vec<usize>, mods: &[Modification], active: u32) -> Vec<ListEntry> {
        let n = active as usize;
        let mut list: Vec<ListEntry> = initial.into_iter().map(Some).collect();
        list.resize(n.max(list.len()), None);
        list.truncate(n);
        let max_pic_num = sps.max_frame_num() as i32;
        let current = sh.frame_num as i32;
        let mut pred = current;
        for (ref_idx, m) in mods.iter().enumerate() {
            if ref_idx >= n {
                break;
            }
            let target = match m.idc {
                0 | 1 => {
                    let abs = m.value as i32 + 1;
                    let mut no_wrap = if m.idc == 0 { pred - abs } else { pred + abs };
                    if no_wrap < 0 {
                        no_wrap += max_pic_num;
                    } else if no_wrap >= max_pic_num {
                        no_wrap -= max_pic_num;
                    }
                    pred = no_wrap;
                    let pic_num = if no_wrap > current { no_wrap - max_pic_num } else { no_wrap };
                    (0..self.pictures.len()).find(|i| self.pictures[*i].reference == Reference::Short && self.pic_num(sps, self.pictures[*i].frame_num, sh.frame_num) == pic_num)
                }
                _ => (0..self.pictures.len()).find(|i| self.pictures[*i].reference == Reference::Long && self.pictures[*i].long_term_frame_idx == m.value),
            };
            // Put it at ref_idx, and take any later copy of it out (8-37, 8-38).
            list.insert(ref_idx, target);
            let mut kept = ref_idx + 1;
            for i in ref_idx + 1..list.len() {
                if list[i] != target || target.is_none() {
                    list[kept] = list[i];
                    kept += 1;
                }
            }
            list.truncate(kept.min(n));
            list.resize(n, None);
        }
        list
    }

    /// Short-term references out of room: the oldest goes (8.2.5.3).
    fn sliding_window(&mut self, sps: &Sps, current_frame_num: u32) {
        let refs = self.pictures.iter().filter(|p| p.reference != Reference::None).count();
        if refs < sps.max_num_ref_frames.max(1) as usize {
            return;
        }
        let oldest = (0..self.pictures.len())
            .filter(|i| self.pictures[*i].reference == Reference::Short)
            .min_by_key(|i| self.pic_num(sps, self.pictures[*i].frame_num, current_frame_num));
        if let Some(i) = oldest {
            self.pictures[i].reference = Reference::None;
        }
    }

    /// Marks references after the current picture's decoded (8.2.5), puts it in the buffer,
    /// and lets go of what's no longer needed. Its POC is reset here on an MMCO 5.
    pub fn add(&mut self, sps: &Sps, sh: &SliceHeader, mut current: Picture) {
        if sh.idr {
            for p in &mut self.pictures {
                p.reference = Reference::None;
            }
            if sh.long_term_reference {
                current.reference = Reference::Long;
                current.long_term_frame_idx = 0;
                self.max_long_term_frame_idx = Some(0);
            } else {
                current.reference = Reference::Short;
                self.max_long_term_frame_idx = None;
            }
        } else if sh.nal_ref_idc != 0 {
            current.reference = Reference::Short;
            match &sh.mmcos {
                None => self.sliding_window(sps, sh.frame_num),
                Some(ops) => {
                    for m in ops {
                        self.mmco(sps, sh, m, &mut current);
                    }
                }
            }
        } else {
            current.reference = Reference::None;
        }
        if sh.has_mmco5() {
            let lowest = current.top_poc.min(current.bottom_poc);
            current.top_poc -= lowest;
            current.bottom_poc -= lowest;
            current.frame_num = 0;
        }
        self.poc.after(sps, sh, (current.top_poc, current.bottom_poc));
        if sh.nal_ref_idc != 0 {
            self.prev_ref_frame_num = if sh.has_mmco5() { 0 } else { sh.frame_num };
        }
        self.pictures.push(current);
    }

    fn mmco(&mut self, sps: &Sps, sh: &SliceHeader, m: &Mmco, current: &mut Picture) {
        let curr_pic_num = sh.frame_num as i32;
        let short_with = |dpb: &Dpb, pic_num: i32| (0..dpb.pictures.len()).find(|i| dpb.pictures[*i].reference == Reference::Short && dpb.pic_num(sps, dpb.pictures[*i].frame_num, sh.frame_num) == pic_num);
        match m.op {
            1 => {
                if let Some(i) = short_with(self, curr_pic_num - (m.difference_of_pic_nums_minus1 as i32 + 1)) {
                    self.pictures[i].reference = Reference::None;
                }
            }
            2 => {
                if let Some(p) = self.pictures.iter_mut().find(|p| p.reference == Reference::Long && p.long_term_frame_idx == m.long_term_pic_num) {
                    p.reference = Reference::None;
                }
            }
            3 => {
                if let Some(p) = self.pictures.iter_mut().find(|p| p.reference == Reference::Long && p.long_term_frame_idx == m.long_term_frame_idx) {
                    p.reference = Reference::None;
                }
                if let Some(i) = short_with(self, curr_pic_num - (m.difference_of_pic_nums_minus1 as i32 + 1)) {
                    self.pictures[i].reference = Reference::Long;
                    self.pictures[i].long_term_frame_idx = m.long_term_frame_idx;
                }
            }
            4 => {
                let max = m.max_long_term_frame_idx_plus1.checked_sub(1);
                for p in &mut self.pictures {
                    if p.reference == Reference::Long && max.is_none_or(|max| p.long_term_frame_idx > max) {
                        p.reference = Reference::None;
                    }
                }
                self.max_long_term_frame_idx = max;
            }
            5 => {
                for p in &mut self.pictures {
                    p.reference = Reference::None;
                }
                self.max_long_term_frame_idx = None;
            }
            6 => {
                if let Some(p) = self.pictures.iter_mut().find(|p| p.reference == Reference::Long && p.long_term_frame_idx == m.long_term_frame_idx) {
                    p.reference = Reference::None;
                }
                current.reference = Reference::Long;
                current.long_term_frame_idx = m.long_term_frame_idx;
            }
            _ => {}
        }
    }

    /// Pictures to show now, in order (C.4.5.3): while more are waiting than may be reordered
    /// (or the buffer's full), the lowest POC goes. `all` empties it (end of stream, IDR).
    pub fn bump(&mut self, reorder: usize, capacity: usize, all: bool) -> Vec<Picture> {
        let mut out = Vec::new();
        loop {
            let waiting = self.pictures.iter().filter(|p| p.output_needed).count();
            let held = self.pictures.len();
            if waiting == 0 || (!all && waiting <= reorder && held <= capacity) {
                break;
            }
            let Some(i) = (0..self.pictures.len()).filter(|i| self.pictures[*i].output_needed).min_by_key(|i| self.pictures[*i].poc()) else { break };
            self.pictures[i].output_needed = false;
            out.push(self.pictures[i]);
            self.prune();
        }
        self.prune();
        out
    }

    /// Lets go of pictures neither referenced nor waiting to be shown.
    pub fn prune(&mut self) {
        self.pictures.retain(|p| p.reference != Reference::None || p.output_needed);
    }

    /// Whether `surface` is still needed (referenced, or not yet shown).
    pub fn holds(&self, surface: u32) -> bool {
        self.pictures.iter().any(|p| !p.non_existing && p.surface == surface)
    }

    /// The references, for the picture parameters' `ReferenceFrames`.
    pub fn references(&self) -> impl Iterator<Item = &Picture> {
        self.pictures.iter().filter(|p| p.reference != Reference::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_and_more_data() {
        // 1 (ue 0), 010 (ue 1), 011 (ue 2), 00100 (ue 3), then the stop bit.
        let data = [0b1010_0110, 0b0100_1000];
        let mut b = Bits::new(&data);
        assert_eq!((b.ue().unwrap(), b.ue().unwrap(), b.ue().unwrap(), b.ue().unwrap()), (0, 1, 2, 3));
        assert!(!b.more_rbsp_data());
    }

    #[test]
    fn emulation_prevention_comes_out() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 5]), [0, 0, 1, 0, 0, 0, 5]);
    }

    #[test]
    fn a_default_scaling_list_is_signalled_by_a_zero() {
        // delta -8 makes nextScale 0 at once: use the default.
        let mut w = Vec::new();
        // se(-8) = ue(16) = 000010001
        w.extend_from_slice(&[0b0000_1000, 0b1000_0000]);
        let mut b = Bits::new(&w);
        let mut list = [0u8; 16];
        assert!(scaling_list(&mut b, &mut list).unwrap());
    }

    fn sps_with(max_refs: u32, poc_type: u32) -> Sps {
        Sps {
            id: 0,
            profile_idc: 100,
            constraint_flags: 0,
            level_idc: 40,
            chroma_format_idc: 1,
            separate_colour_plane: false,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            qpprime_y_zero_transform_bypass: false,
            scaling: None,
            log2_max_frame_num: 4,
            pic_order_cnt_type: poc_type,
            log2_max_poc_lsb: 6,
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: vec![2],
            max_num_ref_frames: max_refs,
            gaps_in_frame_num_allowed: false,
            width_mbs: 20,
            height_map_units: 15,
            frame_mbs_only: true,
            mb_adaptive_frame_field: false,
            direct_8x8_inference: true,
            crop: [0; 4],
            max_num_reorder_frames: None,
            max_dec_frame_buffering: None,
            full_range: false,
            matrix_coefficients: None,
        }
    }

    fn slice(frame_num: u32, poc_lsb: u32, idr: bool, reference: bool, kind: SliceType) -> SliceHeader {
        SliceHeader {
            nal_ref_idc: u32::from(reference),
            idr,
            first_mb: 0,
            slice_type: kind,
            pps_id: 0,
            frame_num,
            mbaff: false,
            idr_pic_id: 0,
            poc_lsb,
            delta_poc_bottom: 0,
            delta_poc: [0, 0],
            redundant_pic_cnt: 0,
            direct_spatial_mv_pred: false,
            num_ref_idx_l0_active: 2,
            num_ref_idx_l1_active: 2,
            mods_l0: Vec::new(),
            mods_l1: Vec::new(),
            weights: None,
            no_output_of_prior_pics: false,
            long_term_reference: false,
            mmcos: None,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            data_bit_offset: 0,
        }
    }

    fn decode(dpb: &mut Dpb, sps: &Sps, sh: &SliceHeader, surface: u32) -> i32 {
        let (top, bottom) = dpb.poc.compute(sps, sh);
        dpb.add(sps, sh, Picture { surface, frame_num: sh.frame_num, top_poc: top, bottom_poc: bottom, reference: Reference::None, long_term_frame_idx: 0, output_needed: true, non_existing: false, pts: Duration::ZERO });
        top
    }

    #[test]
    fn poc_type_0_wraps_its_lsb() {
        let sps = sps_with(2, 0);
        let mut dpb = Dpb::default();
        // max lsb 64, steps under half of it: 0, 30, 60, then 4 means it went past 64.
        assert_eq!(decode(&mut dpb, &sps, &slice(0, 0, true, true, SliceType::I), 1), 0);
        assert_eq!(decode(&mut dpb, &sps, &slice(1, 30, false, true, SliceType::P), 2), 30);
        assert_eq!(decode(&mut dpb, &sps, &slice(2, 60, false, true, SliceType::P), 3), 60);
        assert_eq!(decode(&mut dpb, &sps, &slice(3, 4, false, true, SliceType::P), 4), 68);
        // A jump of more than half the range is backwards (8-3).
        assert_eq!(decode(&mut dpb, &sps, &slice(4, 40, false, true, SliceType::P), 5), 40);
    }

    #[test]
    fn poc_type_2_follows_frame_numbers() {
        let sps = sps_with(2, 2);
        let mut dpb = Dpb::default();
        assert_eq!(decode(&mut dpb, &sps, &slice(0, 0, true, true, SliceType::I), 1), 0);
        assert_eq!(decode(&mut dpb, &sps, &slice(1, 0, false, true, SliceType::P), 2), 2);
        assert_eq!(decode(&mut dpb, &sps, &slice(2, 0, false, false, SliceType::P), 3), 3);
    }

    #[test]
    fn the_sliding_window_keeps_the_newest_references() {
        let sps = sps_with(2, 2);
        let mut dpb = Dpb::default();
        for n in 0..4 {
            decode(&mut dpb, &sps, &slice(n, 0, n == 0, true, if n == 0 { SliceType::I } else { SliceType::P }), n + 1);
        }
        let refs: Vec<u32> = dpb.references().map(|p| p.frame_num).collect();
        assert_eq!(refs, vec![2, 3]);
    }

    #[test]
    fn p_lists_go_newest_first_and_b_lists_by_distance() {
        let sps = sps_with(4, 0);
        let mut dpb = Dpb::default();
        decode(&mut dpb, &sps, &slice(0, 0, true, true, SliceType::I), 1); // poc 0
        decode(&mut dpb, &sps, &slice(1, 8, false, true, SliceType::P), 2); // poc 8
        decode(&mut dpb, &sps, &slice(2, 4, false, true, SliceType::B), 3); // poc 4
        let p = slice(3, 12, false, true, SliceType::P);
        let (l0, _) = dpb.ref_lists(&sps, &p, 12);
        let frame_nums: Vec<u32> = l0.iter().map(|e| dpb.pictures[e.unwrap()].frame_num).collect();
        assert_eq!(frame_nums, vec![2, 1], "PicNum descending");
        let b = slice(3, 6, false, false, SliceType::B);
        let (l0, l1) = dpb.ref_lists(&sps, &b, 6);
        let poc = |l: &[ListEntry]| l.iter().map(|e| dpb.pictures[e.unwrap()].poc()).collect::<Vec<_>>();
        assert_eq!(poc(&l0), vec![4, 0], "before it, nearest first");
        assert_eq!(poc(&l1), vec![8, 4], "after it, nearest first");
    }

    #[test]
    fn a_modification_moves_a_reference_to_the_front() {
        let sps = sps_with(4, 2);
        let mut dpb = Dpb::default();
        for n in 0..3 {
            decode(&mut dpb, &sps, &slice(n, 0, n == 0, true, if n == 0 { SliceType::I } else { SliceType::P }), n + 1);
        }
        let mut p = slice(3, 0, false, true, SliceType::P);
        p.num_ref_idx_l0_active = 3;
        // abs_diff_pic_num_minus1 = 2: PicNum 3 - 3 = 0, the IDR.
        p.mods_l0 = vec![Modification { idc: 0, value: 2 }];
        let (l0, _) = dpb.ref_lists(&sps, &p, 6);
        let frame_nums: Vec<u32> = l0.iter().map(|e| dpb.pictures[e.unwrap()].frame_num).collect();
        assert_eq!(frame_nums, vec![0, 2, 1]);
    }

    #[test]
    fn long_term_references_and_mmco_5() {
        let sps = sps_with(4, 0);
        let mut dpb = Dpb::default();
        decode(&mut dpb, &sps, &slice(0, 0, true, true, SliceType::I), 1);
        let mut p = slice(1, 2, false, true, SliceType::P);
        // Make the IDR long-term (index 0), then the current one stays short-term.
        p.mmcos = Some(vec![Mmco { op: 4, max_long_term_frame_idx_plus1: 1, ..Default::default() }, Mmco { op: 3, difference_of_pic_nums_minus1: 0, long_term_frame_idx: 0, ..Default::default() }]);
        decode(&mut dpb, &sps, &p, 2);
        assert_eq!(dpb.references().filter(|r| r.reference == Reference::Long).count(), 1);
        let mut five = slice(2, 6, false, true, SliceType::P);
        five.mmcos = Some(vec![Mmco { op: 5, ..Default::default() }]);
        decode(&mut dpb, &sps, &five, 3);
        let last = dpb.pictures.last().unwrap();
        assert_eq!((last.frame_num, last.poc()), (0, 0), "an MMCO 5 starts counting again");
        assert_eq!(dpb.references().count(), 1, "everything else let go");
    }

    #[test]
    fn pictures_come_out_in_order() {
        let sps = sps_with(2, 0);
        let mut dpb = Dpb::default();
        let mut shown = Vec::new();
        for (n, (lsb, kind, reference)) in [(0, SliceType::I, true), (8, SliceType::P, true), (4, SliceType::B, false), (16, SliceType::P, true), (12, SliceType::B, false)].into_iter().enumerate() {
            let frame_num = [0, 1, 2, 2, 3][n];
            decode(&mut dpb, &sps, &slice(frame_num, lsb, n == 0, reference, kind), n as u32 + 1);
            shown.extend(dpb.bump(1, 4, false).iter().map(|p| p.poc()));
        }
        shown.extend(dpb.bump(1, 4, true).iter().map(|p| p.poc()));
        assert_eq!(shown, vec![0, 4, 8, 12, 16]);
    }
}
