//! `VideoDecoder` over NVDEC: NVIDIA's parser drives the hardware decoder; each picture comes
//! back scaled to the display size by the hardware and is copied once into pinned host memory.

use std::collections::VecDeque;
use std::ffi::{c_int, c_ulong, c_void};
use std::time::Duration;

use super::device::{self, Device, check};
use super::size::target_size;
use super::sys::*;
use crate::decode::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, chroma_size};
use crate::demux::{Codec, Packet, StreamInfo};
use crate::hw::select::HwCodec;
use crate::nal::{AnnexB, ParamSetFormat};
use crate::{Error, Result};

/// What the hardware decoder was set up for.
#[derive(Clone, Copy)]
struct Setup {
    codec: cudaVideoCodec,
    bit_depth: u32,
    /// Largest coded size the decoder accepts without being recreated.
    max: (u32, u32),
    coded: (u32, u32),
    display: Rect16,
    target: (u32, u32),
    surfaces: u32,
    matrix: Option<ColorMatrix>,
    full_range: bool,
}

impl Setup {
    fn display_size(&self) -> (u32, u32) {
        ((self.display.right - self.display.left) as u32, (self.display.bottom - self.display.top) as u32)
    }
}

/// Pinned host memory the decoded pictures are copied into.
struct Host {
    ptr: *mut u8,
    len: usize,
}

/// State the parser's callbacks reach through `pUserData`. Lives at a fixed address (a leaked
/// box owned by `NvdecVideoDecoder`) and is only touched through raw-pointer-derived references
/// that never outlive one call.
struct Inner {
    dev: &'static Device,
    decoder: CUvideodecoder,
    setup: Option<Setup>,
    hint: Option<(u32, u32)>,
    /// The hint changed since the decoder's target size was set.
    retarget: bool,
    frames: VecDeque<YuvFrame>,
    host: Host,
    /// The first error raised inside a callback (callbacks can only return 0).
    error: Option<Error>,
    stream_matrix: Option<u8>,
    stream_full_range: Option<bool>,
}

pub struct NvdecVideoDecoder {
    inner: *mut Inner,
    parser: CUvideoparser,
    codec: cudaVideoCodec,
    annexb: Option<AnnexB>,
    /// AV1: the sequence header OBUs from `av1C`, sent before the first keyframe after a start
    /// or flush (containers may keep them only there).
    av1_config: Option<Vec<u8>>,
    send_av1_config: bool,
    scratch: Vec<u8>,
    /// Packets sent since the first keyframe (after a start or flush); `None` before it.
    since_keyframe: Option<u32>,
}

/// Packets after a keyframe by which the parser must have announced the stream format.
const FORMAT_WITHIN_PACKETS: u32 = 3;

// SAFETY: the decoder is used through `&mut self` only; the CUDA context is pushed around every
// call, so the thread that makes a call does not matter. `inner` is owned by this value.
unsafe impl Send for NvdecVideoDecoder {}

impl NvdecVideoDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        let dev = device::get().ok_or(Error::Unsupported("NVDEC"))?;
        let hw = HwCodec::of(stream).ok_or(Error::Unsupported("codec for NVDEC"))?;
        let codec = device::cuda_codec(hw).ok_or(Error::Unsupported("codec for NVDEC"))?;
        let config = stream.extradata.as_deref();
        let annexb = match stream.codec {
            Codec::H264 => config.and_then(|c| AnnexB::from_config(ParamSetFormat::Avcc, c)),
            Codec::Hevc => config.and_then(|c| AnnexB::from_config(ParamSetFormat::Hvcc, c)),
            _ => None,
        };
        let av1_config = (stream.codec == Codec::Av1)
            .then(|| config.filter(|c| c.len() > 4).map(|c| c[4..].to_vec()))
            .flatten();
        let inner = Box::into_raw(Box::new(Inner {
            dev,
            decoder: std::ptr::null_mut(),
            setup: None,
            hint: None,
            retarget: false,
            frames: VecDeque::new(),
            host: Host { ptr: std::ptr::null_mut(), len: 0 },
            error: None,
            stream_matrix: stream.color_matrix,
            stream_full_range: stream.full_range,
        }));
        Ok(Self {
            inner,
            parser: std::ptr::null_mut(),
            codec,
            annexb,
            send_av1_config: av1_config.is_some(),
            av1_config,
            scratch: Vec::new(),
            since_keyframe: None,
        })
    }

    fn inner(&mut self) -> &mut Inner {
        // SAFETY: `inner` is our live box; no other reference exists outside a parse call, and
        // this one ends before the next.
        unsafe { &mut *self.inner }
    }

    /// Creates the parser if needed (context must be current).
    fn ensure_parser(&mut self) -> Result<()> {
        if !self.parser.is_null() {
            return Ok(());
        }
        let mut params: CUVIDPARSERPARAMS = zeroed();
        params.CodecType = self.codec;
        params.ulMaxNumDecodeSurfaces = 1; // the sequence callback returns the real number
        params.ulClockRate = 10_000_000; // timestamps in 100 ns units
        params.ulMaxDisplayDelay = 1;
        params.pUserData = self.inner as *mut c_void;
        params.pfnSequenceCallback = Some(on_sequence);
        params.pfnDecodePicture = Some(on_decode);
        params.pfnDisplayPicture = Some(on_display);
        let api = self.inner().dev.api();
        // SAFETY: valid params; `pUserData` outlives the parser (destroyed before `inner`).
        check(unsafe { (api.cuvidCreateVideoParser)(&mut self.parser, &mut params) }, "create parser")
    }

    /// Feeds `payload` to the parser (context must be current); callbacks run inside.
    fn parse(&mut self, payload: &[u8], flags: c_ulong, pts: Duration) -> Result<()> {
        let api = self.inner().dev.api();
        let mut packet: CUVIDSOURCEDATAPACKET = zeroed();
        packet.flags = flags;
        packet.payload_size = payload.len() as c_ulong;
        packet.payload = if payload.is_empty() { std::ptr::null() } else { payload.as_ptr() };
        packet.timestamp = (pts.as_nanos() / 100) as CUvideotimestamp;
        // SAFETY: the parser is live; `payload` outlives the call; no reference to `inner` is
        // held across it (the callbacks make their own).
        let r = unsafe { (api.cuvidParseVideoData)(self.parser, &mut packet) };
        if let Some(e) = self.inner().error.take() {
            return Err(e);
        }
        check(r, "parse")
    }

    fn destroy_parser(&mut self) {
        if !self.parser.is_null() {
            let api = self.inner().dev.api();
            // SAFETY: a live parser, destroyed once.
            unsafe { (api.cuvidDestroyVideoParser)(self.parser) };
            self.parser = std::ptr::null_mut();
        }
    }
}

impl VideoDecoder for NvdecVideoDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let dev = self.inner().dev;
        let _current = dev.push()?;
        self.ensure_parser()?;
        if packet.keyframe {
            self.inner().apply_retarget();
        }
        let mut data = std::mem::take(&mut self.scratch);
        data.clear();
        if let Some(a) = &self.annexb {
            a.convert(&packet.data, packet.keyframe, &mut data)?;
        } else {
            if self.send_av1_config && packet.keyframe {
                data.extend_from_slice(self.av1_config.as_deref().unwrap_or_default());
                self.send_av1_config = false;
            }
            data.extend_from_slice(&packet.data);
        }
        let result = self.parse(&data, CUVID_PKT_TIMESTAMP, packet.pts);
        self.scratch = data;
        result?;
        // NVIDIA's parser silently skips stream variants it can't decode (e.g. VP9 profile 1,
        // 4:4:4) instead of reporting them. It announces the format once the first picture is
        // complete, i.e. within a few packets of a keyframe; if it hasn't, it never will.
        self.since_keyframe = match self.since_keyframe {
            None if packet.keyframe => Some(0),
            n => n.map(|n| n + 1),
        };
        if self.since_keyframe.is_some_and(|n| n >= FORMAT_WITHIN_PACKETS) && self.inner().setup.is_none() {
            return Err(Error::Decode("NVDEC can't decode this stream variant".into()));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        Ok(self.inner().frames.pop_front().map(DecodedFrame::Yuv))
    }

    fn flush(&mut self) {
        let dev = self.inner().dev;
        if let Ok(_current) = dev.push() {
            self.destroy_parser();
        }
        let inner = self.inner();
        inner.frames.clear();
        inner.error = None;
        self.send_av1_config = self.av1_config.is_some();
        self.since_keyframe = None;
    }

    fn send_eof(&mut self) {
        if self.parser.is_null() {
            return;
        }
        let dev = self.inner().dev;
        let Ok(_current) = dev.push() else { return };
        if let Err(e) = self.parse(&[], CUVID_PKT_ENDOFSTREAM, Duration::ZERO) {
            log::debug!("NVDEC end of stream: {e}");
        }
    }

    fn set_output_hint(&mut self, max: Option<(u32, u32)>) {
        let inner = self.inner();
        if inner.hint != max {
            inner.hint = max;
            inner.retarget = true;
        }
    }
}

impl Drop for NvdecVideoDecoder {
    fn drop(&mut self) {
        let dev = self.inner().dev;
        let current = dev.push();
        if current.is_ok() {
            self.destroy_parser();
            let inner = self.inner();
            inner.destroy_decoder();
            if !inner.host.ptr.is_null() {
                // SAFETY: allocated by `cuMemAllocHost`, freed once.
                unsafe { (dev.api().cuMemFreeHost)(inner.host.ptr as *mut c_void) };
            }
        }
        drop(current);
        // SAFETY: created by `Box::into_raw` in `new`; the parser that referenced it is gone.
        drop(unsafe { Box::from_raw(self.inner) });
    }
}

impl Inner {
    /// Destroys the decoder and forgets its setup, so the next format announcement creates a new
    /// one (also after a failed creation).
    fn destroy_decoder(&mut self) {
        if !self.decoder.is_null() {
            // SAFETY: a live decoder, destroyed once (context current).
            unsafe { (self.dev.api().cuvidDestroyDecoder)(self.decoder) };
            self.decoder = std::ptr::null_mut();
        }
        self.setup = None;
    }

    fn live_decoder(&self) -> Result<CUvideodecoder> {
        if self.decoder.is_null() {
            Err(Error::Decode("NVDEC: no decoder for this part of the stream".into()))
        } else {
            Ok(self.decoder)
        }
    }

    /// New stream format (first keyframe, or a change): returns the decode surface count.
    fn on_sequence(&mut self, fmt: &CUVIDEOFORMAT) -> Result<c_int> {
        if fmt.chroma_format != CHROMA_420 {
            return Err(Error::Unsupported("NVDEC output other than 4:2:0"));
        }
        let bit_depth = fmt.bit_depth_luma_minus8 as u32 + 8;
        let coded = (fmt.coded_width, fmt.coded_height);
        let a = fmt.display_area;
        let display = Rect16 { left: a.left as i16, top: a.top as i16, right: a.right as i16, bottom: a.bottom as i16 };
        let display_size = ((a.right - a.left) as u32, (a.bottom - a.top) as u32);
        let target = target_size(display_size, self.hint);
        let surfaces = fmt.min_num_decode_surfaces as u32 + 3; // headroom so mapping never stalls decoding
        let signal = fmt.video_signal_description;
        let matrix = ColorMatrix::from_h273(signal.matrix_coefficients)
            .or_else(|| self.stream_matrix.and_then(ColorMatrix::from_h273));
        let full_range = signal.full_range() || self.stream_full_range == Some(true);
        if let Some(s) = self.setup
            && s.codec == fmt.codec
            && s.bit_depth == bit_depth
            && coded.0 <= s.max.0
            && coded.1 <= s.max.1
            && surfaces <= s.surfaces
        {
            self.reconfigure(coded, display, target, s.surfaces)?;
            self.setup = Some(Setup { coded, display, target, matrix, full_range, ..s });
            self.retarget = false;
            return Ok(s.surfaces as c_int);
        }
        self.destroy_decoder();
        let mut info: CUVIDDECODECREATEINFO = zeroed();
        info.ulWidth = coded.0 as c_ulong;
        info.ulHeight = coded.1 as c_ulong;
        info.ulNumDecodeSurfaces = surfaces as c_ulong;
        info.CodecType = fmt.codec;
        info.ChromaFormat = CHROMA_420;
        info.ulCreationFlags = CREATE_PREFER_CUVID;
        info.bitDepthMinus8 = (bit_depth - 8) as c_ulong;
        info.ulMaxWidth = coded.0 as c_ulong;
        info.ulMaxHeight = coded.1 as c_ulong;
        info.display_area = display;
        info.OutputFormat = if bit_depth > 8 { SURFACE_P016 } else { SURFACE_NV12 };
        info.DeinterlaceMode = if fmt.progressive_sequence != 0 { DEINTERLACE_WEAVE } else { DEINTERLACE_ADAPTIVE };
        info.ulTargetWidth = target.0 as c_ulong;
        info.ulTargetHeight = target.1 as c_ulong;
        info.ulNumOutputSurfaces = 2;
        // SAFETY: valid create info; context current (inside a parse call).
        check(unsafe { (self.dev.api().cuvidCreateDecoder)(&mut self.decoder, &mut info) }, "create decoder")?;
        self.setup = Some(Setup { codec: fmt.codec, bit_depth, max: coded, coded, display, target, surfaces, matrix, full_range });
        self.retarget = false;
        Ok(surfaces as c_int)
    }

    fn reconfigure(&mut self, coded: (u32, u32), display: Rect16, target: (u32, u32), surfaces: u32) -> Result<()> {
        let mut info: CUVIDRECONFIGUREDECODERINFO = zeroed();
        info.ulWidth = coded.0;
        info.ulHeight = coded.1;
        info.ulTargetWidth = target.0;
        info.ulTargetHeight = target.1;
        info.ulNumDecodeSurfaces = surfaces;
        info.display_area = display;
        // SAFETY: a live decoder; context current.
        check(unsafe { (self.dev.api().cuvidReconfigureDecoder)(self.decoder, &mut info) }, "reconfigure decoder")
    }

    /// Applies a changed output hint before a keyframe (context current). If the driver refuses,
    /// the old size stays; the pipeline's CPU scaler still produces the right size.
    fn apply_retarget(&mut self) {
        if !self.retarget || self.decoder.is_null() {
            return;
        }
        self.retarget = false;
        let Some(s) = self.setup else { return };
        let target = target_size(s.display_size(), self.hint);
        if target == s.target {
            return;
        }
        match self.reconfigure(s.coded, s.display, target, s.surfaces) {
            Ok(()) => self.setup = Some(Setup { target, ..s }),
            Err(e) => log::debug!("NVDEC keeps {:?}: {e}", s.target),
        }
    }

    fn on_decode(&mut self, pic: *mut CUVIDPICPARAMS) -> Result<()> {
        let decoder = self.live_decoder()?;
        // SAFETY: `pic` comes from the parser for this decoder; context current.
        check(unsafe { (self.dev.api().cuvidDecodePicture)(decoder, pic) }, "decode")
    }

    /// A picture is due for display: copy it out (scaled) and queue it.
    fn on_display(&mut self, disp: &CUVIDPARSERDISPINFO) -> Result<()> {
        let s = self.setup.ok_or_else(|| Error::Decode("NVDEC: picture before format".into()))?;
        let decoder = self.live_decoder()?;
        let api = self.dev.api();
        let mut proc: CUVIDPROCPARAMS = zeroed();
        proc.progressive_frame = disp.progressive_frame;
        proc.top_field_first = disp.top_field_first;
        proc.unpaired_field = (disp.repeat_first_field < 0) as c_int;
        let (mut dptr, mut pitch) = (0, 0);
        // SAFETY: valid picture index from the parser; out-pointers valid; context current.
        check(
            unsafe { (api.cuvidMapVideoFrame64)(decoder, disp.picture_index, &mut dptr, &mut pitch, &mut proc) },
            "map frame",
        )?;
        let copied = self.copy_out(dptr, pitch as usize, &s);
        // SAFETY: unmaps the frame mapped above.
        unsafe { (api.cuvidUnmapVideoFrame64)(decoder, dptr) };
        copied?;
        let pts = Duration::from_nanos(disp.timestamp.max(0) as u64 * 100);
        let frame = self.to_frame(&s, pts);
        self.frames.push_back(frame);
        Ok(())
    }

    /// Copies the mapped picture (NV12/P016, luma then interleaved chroma at `pitch`) into the
    /// pinned buffer, tightly packed.
    fn copy_out(&mut self, dptr: CUdeviceptr, pitch: usize, s: &Setup) -> Result<()> {
        let api = self.dev.api();
        let bps = if s.bit_depth > 8 { 2 } else { 1 };
        let (w, h) = (s.target.0 as usize, s.target.1 as usize);
        let row = w * bps;
        let chroma_rows = h.div_ceil(2);
        let need = row * (h + chroma_rows);
        if self.host.len < need {
            if !self.host.ptr.is_null() {
                // SAFETY: allocated by `cuMemAllocHost`, freed once.
                unsafe { (api.cuMemFreeHost)(self.host.ptr as *mut c_void) };
                self.host = Host { ptr: std::ptr::null_mut(), len: 0 };
            }
            let mut p = std::ptr::null_mut();
            // SAFETY: out-pointer valid; context current.
            check(unsafe { (api.cuMemAllocHost)(&mut p, need) }, "allocate host buffer")?;
            self.host = Host { ptr: p as *mut u8, len: need };
        }
        let copy = |src: CUdeviceptr, dst: *mut u8, rows: usize| -> Result<()> {
            let mut m: CUDA_MEMCPY2D = zeroed();
            m.srcMemoryType = CU_MEMORYTYPE_DEVICE;
            m.srcDevice = src;
            m.srcPitch = pitch;
            m.dstMemoryType = CU_MEMORYTYPE_HOST;
            m.dstHost = dst as *mut c_void;
            m.dstPitch = row;
            m.WidthInBytes = row;
            m.Height = rows;
            // SAFETY: source is the mapped frame (pitch × rows), destination the pinned buffer
            // (sized `need` above).
            check(unsafe { (api.cuMemcpy2D)(&m) }, "copy frame")
        };
        // The chroma plane starts after the (even) target height's luma rows.
        let chroma_src = dptr + (pitch * ((h + 1) & !1)) as CUdeviceptr;
        copy(dptr, self.host.ptr, h)?;
        // SAFETY: `row * h` is within the `need`-byte buffer.
        copy(chroma_src, unsafe { self.host.ptr.add(row * h) }, chroma_rows)
    }

    /// The pinned buffer as an 8-bit I420 frame (P016: the high byte of each sample).
    fn to_frame(&self, s: &Setup, pts: Duration) -> YuvFrame {
        let bps = if s.bit_depth > 8 { 2 } else { 1 };
        let (w, h) = s.target;
        let (cw, ch) = chroma_size(PixelLayout::I420, w, h);
        let row = w as usize * bps;
        // SAFETY: `copy_out` filled `row * (h + ch)` bytes of the buffer.
        let buf = unsafe { std::slice::from_raw_parts(self.host.ptr, row * (h + ch) as usize) };
        let (luma, chroma) = buf.split_at(row * h as usize);
        // P016 samples are little-endian 16-bit with the value in the high bits: keep the high byte.
        let y: Vec<u8> = if bps == 1 { luma.to_vec() } else { luma.chunks_exact(2).map(|c| c[1]).collect() };
        let cw = cw as usize;
        let (mut u, mut v) = (vec![0u8; cw * ch as usize], vec![0u8; cw * ch as usize]);
        // Chroma rows hold `cw` interleaved U,V pairs; filled row by row so the loop vectorizes.
        for ((src, u), v) in chroma.chunks_exact(row).zip(u.chunks_exact_mut(cw)).zip(v.chunks_exact_mut(cw)) {
            for ((pair, u), v) in src[..cw * 2 * bps].chunks_exact(2 * bps).zip(u).zip(v) {
                *u = pair[bps - 1];
                *v = pair[2 * bps - 1];
            }
        }
        let display_height = s.display_size().1;
        YuvFrame {
            width: w,
            height: h,
            layout: PixelLayout::I420,
            planes: [y, u, v],
            strides: [w as usize, cw, cw],
            matrix: s.matrix.unwrap_or_else(|| ColorMatrix::guess_for_height(display_height)),
            full_range: s.full_range,
            pts,
        }
    }
}

/// Runs a callback body on the `Inner` behind `user`, turning errors into the parser's "stop".
///
/// # Safety
/// `user` must be the `Inner` the parser was created with, called synchronously from inside
/// `cuvidParseVideoData` (so no other reference to it is live).
unsafe fn callback(user: *mut c_void, f: impl FnOnce(&mut Inner) -> Result<c_int>) -> c_int {
    // SAFETY: as documented on this function.
    let inner = unsafe { &mut *(user as *mut Inner) };
    if inner.error.is_some() {
        return 0; // an earlier callback in this call failed: stop here
    }
    match f(inner) {
        Ok(n) => n,
        Err(e) => {
            inner.error.get_or_insert(e);
            0
        }
    }
}

unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut CUVIDEOFORMAT) -> c_int {
    // SAFETY: called by the parser with our `Inner` and a valid format.
    unsafe { callback(user, |i| i.on_sequence(&*fmt)) }
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut CUVIDPICPARAMS) -> c_int {
    // SAFETY: called by the parser with our `Inner` and its picture parameters.
    unsafe { callback(user, |i| i.on_decode(pic).map(|()| 1)) }
}

unsafe extern "C" fn on_display(user: *mut c_void, disp: *mut CUVIDPARSERDISPINFO) -> c_int {
    if disp.is_null() {
        return 1; // end of stream
    }
    // SAFETY: called by the parser with our `Inner` and a valid display info.
    unsafe { callback(user, |i| i.on_display(&*disp).map(|()| 1)) }
}
