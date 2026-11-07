//! `VaapiVideoDecoder`: H.264 through VA-API. Each access unit's slices go to the GPU with the
//! parameters VA-API wants filled in (see [`super::h264`] for where they come from); pictures
//! come back from GPU surfaces as I420, in display order.

use std::collections::VecDeque;
use std::time::Duration;

use super::display::{Display, h264_profile};
use super::h264::{self, Dpb, ListEntry, Picture, Pps, Reference, SliceHeader, Sps};
use super::sys::*;
use crate::color::ColorInfo;
use crate::decode::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, YuvPlanes, chroma_size};
use crate::demux::{Codec, Packet, StreamInfo};
use crate::nal::{AnnexB, ParamSetFormat};
use crate::{Error, Result};

/// The GPU's side: a config and a context for one stream size, with the surfaces pictures are
/// decoded into.
struct Session {
    config: VAConfigID,
    context: VAContextID,
    surfaces: Vec<VASurfaceID>,
    /// What it was made for: profile, coded size, surface count.
    key: (VAProfile, u32, u32, usize),
}

/// The picture being decoded: its first slice's header and where it goes.
struct Current {
    header: SliceHeader,
    sps: Sps,
    surface: VASurfaceID,
    top_poc: i32,
    bottom_poc: i32,
    pts: Duration,
}

pub struct VaapiVideoDecoder {
    display: Display,
    annexb: Option<AnnexB>,
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    session: Option<Session>,
    dpb: Dpb,
    current: Option<Current>,
    frames: VecDeque<DecodedFrame>,
    stream_matrix: Option<u8>,
    stream_full_range: Option<bool>,
    scratch: Vec<u8>,
}

// SAFETY: the display and every id belong to this decoder alone, used from one thread at a time.
unsafe impl Send for VaapiVideoDecoder {}

impl VaapiVideoDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        if stream.codec != Codec::H264 {
            return Err(Error::Unsupported("VA-API decodes H.264 only, so far"));
        }
        let display = Display::open()?;
        let annexb = stream.extradata.as_deref().and_then(|c| AnnexB::from_config(ParamSetFormat::Avcc, c));
        Ok(VaapiVideoDecoder {
            display,
            annexb,
            sps: vec![None; 32],
            pps: vec![None; 256],
            session: None,
            dpb: Dpb::default(),
            current: None,
            frames: VecDeque::new(),
            stream_matrix: stream.color_matrix,
            stream_full_range: stream.full_range,
            scratch: Vec::new(),
        })
    }

    fn va(&self) -> &'static Va {
        self.display.va
    }

    /// A config, context and surfaces for `sps`, made again when the stream's size changes.
    fn session_for(&mut self, sps: &Sps) -> Result<()> {
        let have = self.display.decode_profiles();
        let profile = h264_profile(sps.profile_idc, sps.constraint_flags, &have).ok_or(Error::Unsupported("this H.264 profile on this GPU"))?;
        if sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 || sps.chroma_format_idc != 1 {
            return Err(Error::Unsupported("H.264 other than 8-bit 4:2:0 through VA-API"));
        }
        let (w, h) = sps.coded_size();
        // The buffer, the picture being decoded, and two spare for the copy out.
        let count = sps.dpb_frames() + 3;
        let key = (profile, w, h, count);
        if self.session.as_ref().is_some_and(|s| s.key == key) {
            return Ok(());
        }
        self.end_session();
        let (va, dpy) = (self.va(), self.display.dpy);
        let mut config = VA_INVALID_ID;
        let mut attribs = [VAConfigAttrib { type_: VAConfigAttribRTFormat, value: VA_RT_FORMAT_YUV420 }];
        self.display.check(unsafe { (va.vaCreateConfig)(dpy, profile, VAEntrypointVLD, attribs.as_mut_ptr(), 1, &mut config) }, "config")?;
        let mut surfaces = vec![VA_INVALID_SURFACE; count];
        if let Err(e) = self.display.check(unsafe { (va.vaCreateSurfaces)(dpy, VA_RT_FORMAT_YUV420, w, h, surfaces.as_mut_ptr(), count as u32, std::ptr::null_mut(), 0) }, "surfaces") {
            unsafe { (va.vaDestroyConfig)(dpy, config) };
            return Err(e);
        }
        let mut context = VA_INVALID_ID;
        if let Err(e) = self.display.check(unsafe { (va.vaCreateContext)(dpy, config, w as i32, h as i32, VA_PROGRESSIVE as i32, surfaces.as_mut_ptr(), count as i32, &mut context) }, "context") {
            unsafe {
                (va.vaDestroySurfaces)(dpy, surfaces.as_mut_ptr(), count as i32);
                (va.vaDestroyConfig)(dpy, config);
            }
            return Err(e);
        }
        self.dpb.clear();
        self.session = Some(Session { config, context, surfaces, key });
        Ok(())
    }

    fn end_session(&mut self) {
        let Some(mut s) = self.session.take() else { return };
        let (va, dpy) = (self.va(), self.display.dpy);
        unsafe {
            (va.vaDestroyContext)(dpy, s.context);
            (va.vaDestroySurfaces)(dpy, s.surfaces.as_mut_ptr(), s.surfaces.len() as i32);
            (va.vaDestroyConfig)(dpy, s.config);
        }
    }

    /// A surface no picture still needs.
    fn free_surface(&mut self) -> Option<VASurfaceID> {
        let session = self.session.as_ref()?;
        session.surfaces.iter().copied().find(|s| !self.dpb.holds(*s))
    }

    /// Hands VA-API a buffer of `data`; its id.
    fn buffer<T>(&self, kind: VABufferType, data: &mut T) -> Result<VABufferID> {
        self.buffer_bytes(kind, (data as *mut T).cast(), size_of::<T>())
    }

    fn buffer_bytes(&self, kind: VABufferType, data: *mut core::ffi::c_void, size: usize) -> Result<VABufferID> {
        let session = self.session.as_ref().ok_or_else(|| Error::Decode("VA-API: no context".into()))?;
        let mut id = VA_INVALID_ID;
        self.display.check(unsafe { (self.va().vaCreateBuffer)(self.display.dpy, session.context, kind, size as u32, 1, data, &mut id) }, "buffer")?;
        Ok(id)
    }

    /// Renders and lets go of `buffers` (VA-API copies them in).
    fn render(&self, buffers: &mut [VABufferID]) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(|| Error::Decode("VA-API: no context".into()))?;
        let status = unsafe { (self.va().vaRenderPicture)(self.display.dpy, session.context, buffers.as_mut_ptr(), buffers.len() as i32) };
        for id in buffers.iter() {
            unsafe { (self.va().vaDestroyBuffer)(self.display.dpy, *id) };
        }
        self.display.check(status, "render")
    }

    fn handle_nal(&mut self, nal: &[u8], pts: Duration) -> Result<()> {
        let Some(&header) = nal.first() else { return Ok(()) };
        match header & 0x1f {
            7 => {
                let sps = h264::parse_sps(&h264::unescape(&nal[1..]))?;
                let id = sps.id as usize;
                self.sps[id] = Some(sps);
            }
            8 => {
                let pps = h264::parse_pps(&h264::unescape(&nal[1..]), &self.sps)?;
                let id = pps.id as usize;
                self.pps[id] = Some(pps);
            }
            1 | 5 => self.slice(nal, pts)?,
            _ => {}
        }
        Ok(())
    }

    fn slice(&mut self, nal: &[u8], pts: Duration) -> Result<()> {
        let unescaped = h264::unescape(nal);
        let sh = h264::parse_slice_header(&unescaped, &self.sps, &self.pps)?;
        if sh.redundant_pic_cnt > 0 {
            return Ok(());
        }
        if sh.first_mb == 0 && self.current.is_some() {
            self.finish_picture()?;
        }
        if self.current.is_none() {
            self.start_picture(&sh, pts)?;
        }
        self.render_slice(&sh, nal)
    }

    fn start_picture(&mut self, sh: &SliceHeader, pts: Duration) -> Result<()> {
        let pps = self.pps[sh.pps_id as usize].clone().ok_or_else(|| Error::Decode("H.264: PPS missing".into()))?;
        let sps = self.sps[pps.sps_id as usize].clone().ok_or_else(|| Error::Decode("H.264: SPS missing".into()))?;
        self.session_for(&sps)?;
        // An IDR, or an MMCO 5, shows everything before it first (C.4.4), unless told not to.
        if sh.idr || sh.has_mmco5() {
            let flushed = self.dpb.bump(0, 0, true);
            if !sh.no_output_of_prior_pics {
                self.output(&flushed, &sps)?;
            }
            if sh.idr {
                let poc = self.dpb.poc;
                self.dpb.clear();
                self.dpb.poc = poc;
            }
        }
        let surface = match self.free_surface() {
            Some(s) => s,
            None => {
                // Every surface held: show what's waiting, and use what's freed.
                let flushed = self.dpb.bump(0, 0, true);
                self.output(&flushed, &sps)?;
                self.free_surface().ok_or_else(|| Error::Decode("VA-API: out of surfaces".into()))?
            }
        };
        if sps.gaps_in_frame_num_allowed || !sh.idr {
            let stand_in = self.dpb.references().next().map_or(surface, |p| p.surface);
            self.dpb.fill_gap(&sps, sh, stand_in);
        }
        let (top_poc, bottom_poc) = self.dpb.poc.compute(&sps, sh);
        let session = self.session.as_ref().ok_or_else(|| Error::Decode("VA-API: no context".into()))?;
        self.display.check(unsafe { (self.va().vaBeginPicture)(self.display.dpy, session.context, surface) }, "begin")?;
        let mut params = self.picture_params(&sps, &pps, sh, surface, top_poc, bottom_poc);
        let mut matrix = iq_matrix(&pps.scaling(&sps));
        let mut buffers = [self.buffer(VAPictureParameterBufferType, &mut params)?, self.buffer(VAIQMatrixBufferType, &mut matrix)?];
        self.render(&mut buffers)?;
        self.current = Some(Current { header: sh.clone(), sps, surface, top_poc, bottom_poc, pts });
        Ok(())
    }

    fn picture_params(&self, sps: &Sps, pps: &Pps, sh: &SliceHeader, surface: VASurfaceID, top: i32, bottom: i32) -> VAPictureParameterBufferH264 {
        // SAFETY: a C struct of numbers; zero is a valid value for all of it.
        let mut p: VAPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
        p.CurrPic = VAPictureH264 { picture_id: surface, frame_idx: sh.frame_num, flags: 0, TopFieldOrderCnt: top, BottomFieldOrderCnt: bottom, va_reserved: [0; 4] };
        p.ReferenceFrames = [invalid(); 16];
        for (slot, r) in p.ReferenceFrames.iter_mut().zip(self.dpb.references()) {
            *slot = va_picture(r);
        }
        p.picture_width_in_mbs_minus1 = (sps.width_mbs - 1) as u16;
        p.picture_height_in_mbs_minus1 = (sps.height_mbs() - 1) as u16;
        p.bit_depth_luma_minus8 = (sps.bit_depth_luma - 8) as u8;
        p.bit_depth_chroma_minus8 = (sps.bit_depth_chroma - 8) as u8;
        p.num_ref_frames = sps.max_num_ref_frames as u8;
        // SAFETY: the bits view of the union, all of it zero so far.
        unsafe {
            let s = &mut p.seq_fields.bits;
            s.set_chroma_format_idc(sps.chroma_format_idc);
            s.set_residual_colour_transform_flag(u32::from(sps.separate_colour_plane));
            s.set_gaps_in_frame_num_value_allowed_flag(u32::from(sps.gaps_in_frame_num_allowed));
            s.set_frame_mbs_only_flag(u32::from(sps.frame_mbs_only));
            s.set_mb_adaptive_frame_field_flag(u32::from(sps.mb_adaptive_frame_field));
            s.set_direct_8x8_inference_flag(u32::from(sps.direct_8x8_inference));
            // Level 3.1 and up restrict bi-prediction to 8x8 and larger (Table A-4).
            s.set_MinLumaBiPredSize8x8(u32::from(sps.level_idc >= 31));
            s.set_log2_max_frame_num_minus4(sps.log2_max_frame_num - 4);
            s.set_pic_order_cnt_type(sps.pic_order_cnt_type);
            s.set_log2_max_pic_order_cnt_lsb_minus4(sps.log2_max_poc_lsb.saturating_sub(4));
            s.set_delta_pic_order_always_zero_flag(u32::from(sps.delta_pic_order_always_zero));
            let f = &mut p.pic_fields.bits;
            f.set_entropy_coding_mode_flag(u32::from(pps.entropy_coding_mode));
            f.set_weighted_pred_flag(u32::from(pps.weighted_pred));
            f.set_weighted_bipred_idc(pps.weighted_bipred_idc);
            f.set_transform_8x8_mode_flag(u32::from(pps.transform_8x8_mode));
            f.set_field_pic_flag(0);
            f.set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred));
            f.set_pic_order_present_flag(u32::from(pps.bottom_field_pic_order_in_frame_present));
            f.set_deblocking_filter_control_present_flag(u32::from(pps.deblocking_filter_control_present));
            f.set_redundant_pic_cnt_present_flag(u32::from(pps.redundant_pic_cnt_present));
            f.set_reference_pic_flag(u32::from(sh.nal_ref_idc != 0));
        }
        p.pic_init_qp_minus26 = pps.pic_init_qp_minus26 as i8;
        p.pic_init_qs_minus26 = pps.pic_init_qs_minus26 as i8;
        p.chroma_qp_index_offset = pps.chroma_qp_index_offset as i8;
        p.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset as i8;
        p.frame_num = sh.frame_num as u16;
        p
    }

    fn render_slice(&mut self, sh: &SliceHeader, nal: &[u8]) -> Result<()> {
        let current = self.current.as_ref().ok_or_else(|| Error::Decode("VA-API: slice outside a picture".into()))?;
        let poc = current.top_poc.min(current.bottom_poc);
        let (l0, l1) = self.dpb.ref_lists(&current.sps, sh, poc);
        // SAFETY: a C struct of numbers; zero is valid for all of it.
        let mut p: VASliceParameterBufferH264 = unsafe { std::mem::zeroed() };
        p.slice_data_size = nal.len() as u32;
        p.slice_data_offset = 0;
        p.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
        p.slice_data_bit_offset = sh.data_bit_offset as u16;
        p.first_mb_in_slice = sh.first_mb as u16;
        p.slice_type = sh.slice_type.number();
        p.direct_spatial_mv_pred_flag = u8::from(sh.direct_spatial_mv_pred);
        p.num_ref_idx_l0_active_minus1 = sh.num_ref_idx_l0_active.saturating_sub(1) as u8;
        p.num_ref_idx_l1_active_minus1 = sh.num_ref_idx_l1_active.saturating_sub(1) as u8;
        p.cabac_init_idc = sh.cabac_init_idc as u8;
        p.slice_qp_delta = sh.slice_qp_delta as i8;
        p.disable_deblocking_filter_idc = sh.disable_deblocking_filter_idc as u8;
        p.slice_alpha_c0_offset_div2 = sh.slice_alpha_c0_offset_div2 as i8;
        p.slice_beta_offset_div2 = sh.slice_beta_offset_div2 as i8;
        p.RefPicList0 = [invalid(); 32];
        p.RefPicList1 = [invalid(); 32];
        let entry = |e: &ListEntry| e.map_or(invalid(), |i| va_picture(&self.dpb.pictures[i]));
        for (slot, e) in p.RefPicList0.iter_mut().zip(&l0) {
            *slot = entry(e);
        }
        for (slot, e) in p.RefPicList1.iter_mut().zip(&l1) {
            *slot = entry(e);
        }
        if let Some(w) = &sh.weights {
            p.luma_log2_weight_denom = w.luma_log2_denom as u8;
            p.chroma_log2_weight_denom = w.chroma_log2_denom as u8;
            let (luma_default, chroma_default) = (1i32 << w.luma_log2_denom, 1i32 << w.chroma_log2_denom);
            p.luma_weight_l0_flag = u8::from(w.l0.iter().any(|x| x.luma.is_some()));
            p.chroma_weight_l0_flag = u8::from(w.l0.iter().any(|x| x.chroma.is_some()));
            p.luma_weight_l1_flag = u8::from(w.l1.iter().any(|x| x.luma.is_some()));
            p.chroma_weight_l1_flag = u8::from(w.l1.iter().any(|x| x.chroma.is_some()));
            for (i, x) in w.l0.iter().enumerate().take(32) {
                let (lw, lo) = x.luma.unwrap_or((luma_default, 0));
                (p.luma_weight_l0[i], p.luma_offset_l0[i]) = (lw as i16, lo as i16);
                let c = x.chroma.unwrap_or([(chroma_default, 0); 2]);
                for (j, (weight, offset)) in c.iter().enumerate() {
                    (p.chroma_weight_l0[i][j], p.chroma_offset_l0[i][j]) = (*weight as i16, *offset as i16);
                }
            }
            for (i, x) in w.l1.iter().enumerate().take(32) {
                let (lw, lo) = x.luma.unwrap_or((luma_default, 0));
                (p.luma_weight_l1[i], p.luma_offset_l1[i]) = (lw as i16, lo as i16);
                let c = x.chroma.unwrap_or([(chroma_default, 0); 2]);
                for (j, (weight, offset)) in c.iter().enumerate() {
                    (p.chroma_weight_l1[i][j], p.chroma_offset_l1[i][j]) = (*weight as i16, *offset as i16);
                }
            }
        }
        let params = self.buffer(VASliceParameterBufferType, &mut p)?;
        let data = self.buffer_bytes(VASliceDataBufferType, nal.as_ptr().cast_mut().cast(), nal.len());
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                unsafe { (self.va().vaDestroyBuffer)(self.display.dpy, params) };
                return Err(e);
            }
        };
        self.render(&mut [params, data])
    }

    /// Ends the picture: decoded, marked, and whatever's due shown.
    fn finish_picture(&mut self) -> Result<()> {
        let Some(current) = self.current.take() else { return Ok(()) };
        let session = self.session.as_ref().ok_or_else(|| Error::Decode("VA-API: no context".into()))?;
        self.display.check(unsafe { (self.va().vaEndPicture)(self.display.dpy, session.context) }, "end")?;
        let picture = Picture {
            surface: current.surface,
            frame_num: current.header.frame_num,
            top_poc: current.top_poc,
            bottom_poc: current.bottom_poc,
            reference: Reference::None,
            long_term_frame_idx: 0,
            output_needed: true,
            non_existing: false,
            pts: current.pts,
        };
        self.dpb.add(&current.sps, &current.header, picture);
        let due = self.dpb.bump(current.sps.reorder_depth(), current.sps.dpb_frames(), false);
        self.output(&due, &current.sps)
    }

    /// Copies pictures off the GPU, in the order given.
    fn output(&mut self, pictures: &[Picture], sps: &Sps) -> Result<()> {
        for p in pictures.iter().filter(|p| !p.non_existing) {
            let frame = self.read(p.surface, sps, p.pts)?;
            self.frames.push_back(DecodedFrame::Yuv(frame));
        }
        Ok(())
    }

    /// A surface's picture as I420, cropped as the SPS says.
    fn read(&self, surface: VASurfaceID, sps: &Sps, pts: Duration) -> Result<YuvFrame> {
        let (va, dpy) = (self.va(), self.display.dpy);
        self.display.check(unsafe { (va.vaSyncSurface)(dpy, surface) }, "sync")?;
        let (coded_w, coded_h) = sps.coded_size();
        // SAFETY: a C struct of numbers, filled by libva.
        let mut image: VAImage = unsafe { std::mem::zeroed() };
        let derived = unsafe { (va.vaDeriveImage)(dpy, surface, &mut image) } == VA_STATUS_SUCCESS as i32 && image.format.fourcc == VA_FOURCC_NV12;
        if !derived {
            if image.image_id != 0 && image.image_id != VA_INVALID_ID {
                unsafe { (va.vaDestroyImage)(dpy, image.image_id) };
            }
            // No direct view of the surface (or not NV12): copied into an image of our own.
            let mut format: VAImageFormat = unsafe { std::mem::zeroed() };
            format.fourcc = VA_FOURCC_NV12;
            format.byte_order = 1;
            format.bits_per_pixel = 12;
            self.display.check(unsafe { (va.vaCreateImage)(dpy, &mut format, coded_w as i32, coded_h as i32, &mut image) }, "image")?;
            if let Err(e) = self.display.check(unsafe { (va.vaGetImage)(dpy, surface, 0, 0, coded_w, coded_h, image.image_id) }, "get image") {
                unsafe { (va.vaDestroyImage)(dpy, image.image_id) };
                return Err(e);
            }
        }
        let mut mapped = std::ptr::null_mut();
        let result = self.display.check(unsafe { (va.vaMapBuffer)(dpy, image.buf, &mut mapped) }, "map").map(|()| {
            let (x, y, w, h) = sps.display_rect();
            // SAFETY: the mapped image holds `data_size` bytes, planes at their offsets and pitches.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), image.data_size as usize) };
            let frame = self.to_i420(bytes, &image, sps, (x, y, w, h), pts);
            unsafe { (va.vaUnmapBuffer)(dpy, image.buf) };
            frame
        });
        unsafe { (va.vaDestroyImage)(dpy, image.image_id) };
        result
    }

    fn to_i420(&self, bytes: &[u8], image: &VAImage, sps: &Sps, (x, y, w, h): (u32, u32, u32, u32), pts: Duration) -> YuvFrame {
        let (w, h) = (w & !1, h & !1);
        let (cw, ch) = chroma_size(PixelLayout::I420, w, h);
        let (y_pitch, uv_pitch) = (image.pitches[0] as usize, image.pitches[1] as usize);
        let (y_off, uv_off) = (image.offsets[0] as usize, image.offsets[1] as usize);
        let mut luma = Vec::with_capacity((w * h) as usize);
        for row in 0..h as usize {
            let start = y_off + (row + y as usize) * y_pitch + x as usize;
            luma.extend_from_slice(&bytes[start..start + w as usize]);
        }
        let (mut u, mut v) = (Vec::with_capacity((cw * ch) as usize), Vec::with_capacity((cw * ch) as usize));
        for row in 0..ch as usize {
            let start = uv_off + (row + y as usize / 2) * uv_pitch + (x as usize / 2) * 2;
            for pair in bytes[start..start + cw as usize * 2].as_chunks::<2>().0 {
                u.push(pair[0]);
                v.push(pair[1]);
            }
        }
        let matrix = sps.matrix_coefficients.and_then(ColorMatrix::from_h273).or_else(|| self.stream_matrix.and_then(ColorMatrix::from_h273)).unwrap_or_else(|| ColorMatrix::guess_for_height(h));
        let full_range = sps.full_range || self.stream_full_range == Some(true);
        YuvFrame { width: w, height: h, layout: PixelLayout::I420, planes: YuvPlanes::U8([luma, u, v]), strides: [w as usize, cw as usize, cw as usize], bit_depth: 8, color: ColorInfo::sdr(matrix, full_range), pts }
    }
}

fn invalid() -> VAPictureH264 {
    VAPictureH264 { picture_id: VA_INVALID_SURFACE, frame_idx: 0, flags: VA_PICTURE_H264_INVALID, TopFieldOrderCnt: 0, BottomFieldOrderCnt: 0, va_reserved: [0; 4] }
}

fn va_picture(p: &Picture) -> VAPictureH264 {
    let (flags, frame_idx) = match p.reference {
        Reference::Long => (VA_PICTURE_H264_LONG_TERM_REFERENCE, p.long_term_frame_idx),
        _ => (VA_PICTURE_H264_SHORT_TERM_REFERENCE, p.frame_num),
    };
    VAPictureH264 { picture_id: p.surface, frame_idx, flags, TopFieldOrderCnt: p.top_poc, BottomFieldOrderCnt: p.bottom_poc, va_reserved: [0; 4] }
}

/// Zig-zag (frame) scan position to raster position, 4x4 and 8x8 (8.5.6, Table 8-13).
const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61,
    54, 47, 55, 62, 63,
];

/// A list in the order sent (zig-zag) put in raster order.
fn raster<const N: usize>(list: &[u8; N], scan: &[usize; N]) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, value) in list.iter().enumerate() {
        out[scan[i]] = *value;
    }
    out
}

fn iq_matrix(s: &h264::Scaling) -> VAIQMatrixBufferH264 {
    // VA-API takes the six 4x4 lists and the two luma 8x8 lists in raster order (va.h); the
    // bitstream, and our lists, have them in zig-zag order.
    VAIQMatrixBufferH264 {
        ScalingList4x4: s.x4.map(|l| raster(&l, &ZIGZAG_4X4)),
        ScalingList8x8: [raster(&s.x8[0], &ZIGZAG_8X8), raster(&s.x8[1], &ZIGZAG_8X8)],
        va_reserved: [0; 4],
    }
}


impl VideoDecoder for VaapiVideoDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let mut data = std::mem::take(&mut self.scratch);
        data.clear();
        match &self.annexb {
            Some(a) => a.convert(&packet.data, packet.keyframe, &mut data)?,
            None => data.extend_from_slice(&packet.data),
        }
        let result = (|| {
            for nal in crate::nal::split_annex_b(&data) {
                self.handle_nal(nal, packet.pts)?;
            }
            // One packet is one access unit.
            self.finish_picture()
        })();
        self.scratch = data;
        if result.is_err() {
            self.current = None;
        }
        result
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        Ok(self.frames.pop_front())
    }

    fn flush(&mut self) {
        if self.current.take().is_some()
            && let Some(s) = &self.session
        {
            // A picture begun but not ended: end it so the context can take the next one.
            unsafe { (self.va().vaEndPicture)(self.display.dpy, s.context) };
        }
        let poc = h264::PocState::default();
        self.dpb.clear();
        self.dpb.poc = poc;
        self.frames.clear();
    }

    fn send_eof(&mut self) {
        if let Err(e) = self.finish_picture() {
            log::debug!("VA-API: last picture: {e}");
        }
        let Some(sps) = self.sps.iter().flatten().next().cloned() else { return };
        let rest = self.dpb.bump(0, 0, true);
        if let Err(e) = self.output(&rest, &sps) {
            log::debug!("VA-API: last pictures: {e}");
        }
    }
}

impl Drop for VaapiVideoDecoder {
    fn drop(&mut self) {
        self.flush();
        self.end_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_tables_are_permutations() {
        let mut a = ZIGZAG_4X4;
        a.sort_unstable();
        assert_eq!(a, core::array::from_fn::<usize, 16, _>(|i| i));
        let mut b = ZIGZAG_8X8;
        b.sort_unstable();
        assert_eq!(b, core::array::from_fn::<usize, 64, _>(|i| i));
    }

    #[test]
    fn the_default_intra_list_comes_out_in_raster_order() {
        // Table 7-3's Default_4x4_Intra, as the spec's raster matrix has it.
        let zigzag = [6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42];
        assert_eq!(raster(&zigzag, &ZIGZAG_4X4), [6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42]);
    }
}
