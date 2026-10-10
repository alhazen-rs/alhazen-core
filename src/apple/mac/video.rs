//! `VtVideoDecoder`: VideoToolbox decompression sessions, pictures copied out as I420.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::time::Duration;

use objc2_core_foundation::{CFRetained, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMVideoFormatDescription, CMVideoFormatDescriptionCreate,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMTimeInvalid,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_422YpCbCr8BiPlanarFullRange, kCVPixelFormatType_422YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_422YpCbCr10BiPlanarFullRange, kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr8BiPlanarFullRange, kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr10BiPlanarFullRange, kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange,
};
use objc2_video_toolbox::{VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession};

use super::cf;
use crate::apple::format::{atom_name, prores_subtype, video_codec_type, vpcc_from_keyframe};
use crate::apple::planes::biplanar_to_planar;
use crate::decode::{ColorMatrix, DecodedFrame, PixelLayout, VideoDecoder, YuvFrame, chroma_size};
use crate::demux::{Codec, Packet, StreamInfo};
use crate::nvdec::size::target_size;
use crate::{Error, Result};

/// The bi-planar output formats we ask for and read.
struct OutputFormat {
    code: u32,
    layout: PixelLayout,
    ten_bit: bool,
    full_range: bool,
}

const FORMATS: [OutputFormat; 12] = {
    const fn f(code: u32, layout: PixelLayout, ten_bit: bool, full_range: bool) -> OutputFormat {
        OutputFormat { code, layout, ten_bit, full_range }
    }
    use PixelLayout::{I420, I422, I444};
    [
        f(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, I420, false, false),
        f(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, I420, false, true),
        f(kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, I420, true, false),
        f(kCVPixelFormatType_420YpCbCr10BiPlanarFullRange, I420, true, true),
        f(kCVPixelFormatType_422YpCbCr8BiPlanarVideoRange, I422, false, false),
        f(kCVPixelFormatType_422YpCbCr8BiPlanarFullRange, I422, false, true),
        f(kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange, I422, true, false),
        f(kCVPixelFormatType_422YpCbCr10BiPlanarFullRange, I422, true, true),
        f(kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange, I444, false, false),
        f(kCVPixelFormatType_444YpCbCr8BiPlanarFullRange, I444, false, true),
        f(kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange, I444, true, false),
        f(kCVPixelFormatType_444YpCbCr10BiPlanarFullRange, I444, true, true),
    ]
};

/// Timestamps are passed in nanoseconds.
const TIMESCALE: i32 = 1_000_000_000;

/// Where the output callback leaves pictures (called from VideoToolbox's threads).
struct Sink {
    frames: Mutex<Vec<YuvFrame>>,
    error: Mutex<Option<String>>,
    matrix: Option<ColorMatrix>,
    display_height: u32,
}

/// A live session; invalidated (and its callback context freed) when dropped.
struct Session {
    vt: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMVideoFormatDescription>,
    /// Freed after the session is invalidated: the callback's context.
    sink: Box<Sink>,
    /// The output size the session scales to.
    target: (u32, u32),
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a live session; no callback runs after this returns.
        unsafe { self.vt.invalidate() };
    }
}

pub struct VtVideoDecoder {
    stream: StreamInfo,
    session: Option<Session>,
    hint: Option<(u32, u32)>,
    /// Decoded pictures, in presentation order (VideoToolbox's temporal processing delivers them
    /// so; each batch is sorted too).
    window: Vec<YuvFrame>,
    eof: bool,
}

// SAFETY: the session is created and used only through `&mut self`, on the decode thread; the
// callback's shared state is behind mutexes.
unsafe impl Send for VtVideoDecoder {}

impl VtVideoDecoder {
    pub fn new(stream: &StreamInfo) -> Result<Self> {
        if video_codec_type(&stream.codec).is_none() && stream.codec != Codec::ProRes {
            return Err(Error::Unsupported("codec for VideoToolbox"));
        }
        Ok(Self { stream: stream.clone(), session: None, hint: None, window: Vec::new(), eof: false })
    }

    fn display(&self) -> (u32, u32) {
        (self.stream.width, self.stream.height)
    }

    /// A session for the stream, set up from its first keyframe.
    fn create_session(&self, keyframe: &[u8]) -> Result<Session> {
        let s = &self.stream;
        let (w, h) = self.display();
        if w == 0 || h == 0 {
            return Err(Error::Decode("VideoToolbox needs the picture size".into()));
        }
        let codec_type = match s.codec {
            Codec::ProRes => prores_subtype(keyframe).ok_or_else(|| Error::Decode("not a ProRes frame".into()))?,
            ref c => video_codec_type(c).ok_or(Error::Unsupported("codec for VideoToolbox"))?,
        };
        let record = match s.codec {
            Codec::Vp9 => Some(vpcc_from_keyframe(keyframe).ok_or_else(|| Error::Decode("VP9 stream starts without a keyframe".into()))?),
            Codec::ProRes => None,
            _ => Some(s.extradata.clone().ok_or_else(|| Error::Decode(format!("{} without its setup record", s.codec)))?),
        };
        let bit_depth = match s.codec {
            Codec::Vp9 => record.as_ref().map_or(8, |r| r[6] as u32 >> 4),
            Codec::ProRes => 10,
            _ => crate::nvdec::profile::format(s).map_or(8, |f| f.bit_depth),
        };
        // The source's chroma sampling, kept in the output (ProRes and 4:4:4 streams lose nothing).
        let layout = match s.codec {
            Codec::Vp9 => match record.as_ref().map_or(1, |r| (r[6] >> 1) & 7) {
                2 => PixelLayout::I422,
                3 => PixelLayout::I444,
                _ => PixelLayout::I420,
            },
            Codec::ProRes => match keyframe.get(20).map(|b| b >> 6) {
                Some(3) => PixelLayout::I444,
                _ => PixelLayout::I422,
            },
            _ => match crate::nvdec::profile::format(s).map(|f| f.chroma) {
                Some(crate::nvdec::profile::Chroma::Yuv422) => PixelLayout::I422,
                Some(crate::nvdec::profile::Chroma::Yuv444) => PixelLayout::I444,
                _ => PixelLayout::I420,
            },
        };

        // Format description: the codec record as a sample description extension atom.
        let mut extensions = None;
        if let (Some(name), Some(record)) = (atom_name(&s.codec), &record) {
            let (key, value) = (cf::string(name), cf::data(record));
            let atoms = cf::dictionary(&[(&key, value.as_ref())]);
            // SAFETY: an immutable framework constant.
            let ext_key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
            extensions = Some(cf::dictionary(&[(ext_key, atoms.as_ref())]));
        }
        let mut format: *const CMVideoFormatDescription = ptr::null();
        // SAFETY: valid arguments; the out-pointer receives an owned description.
        let format = unsafe {
            let status = CMVideoFormatDescriptionCreate(None, codec_type, w as i32, h as i32, extensions.as_deref(), NonNull::from(&mut format));
            cf::created(status, format, "CMVideoFormatDescriptionCreate")?
        };

        // Output: 4:2:0 bi-planar, 8 or 10 bits, the stream's range, scaled to the hint.
        let full_range = s.full_range == Some(true);
        let pixel_format = FORMATS
            .iter()
            .find(|f| f.layout == layout && f.ten_bit == (bit_depth > 8) && f.full_range == full_range)
            .map(|f| f.code)
            .unwrap_or(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
        let target = target_size((w, h), self.hint);
        let format_number = cf::number(pixel_format as i64);
        let (tw, th) = (cf::number(target.0 as i64), cf::number(target.1 as i64));
        // SAFETY: immutable framework constants.
        let (pf_key, w_key, h_key) = unsafe { (kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey) };
        let mut entries: Vec<(&objc2_core_foundation::CFString, &CFType)> = vec![(pf_key, format_number.as_ref())];
        if target != (w, h) {
            entries.push((w_key, tw.as_ref()));
            entries.push((h_key, th.as_ref()));
        }
        let attributes = cf::dictionary(&entries);

        let sink = Box::new(Sink {
            frames: Mutex::new(Vec::new()),
            error: Mutex::new(None),
            matrix: s.color_matrix.and_then(ColorMatrix::from_h273),
            display_height: h,
        });
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_output),
            decompressionOutputRefCon: &*sink as *const Sink as *mut c_void,
        };
        let mut vt: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: valid arguments; `sink` outlives the session (see `Session`).
        let vt = unsafe {
            let status = VTDecompressionSession::create(None, &format, None, Some(&attributes), &callback, NonNull::from(&mut vt));
            cf::created(status, vt, "VTDecompressionSessionCreate")?
        };
        log::info!("VideoToolbox {} {w}x{h} ({bit_depth}-bit {layout:?}) → {}x{}", s.codec, target.0, target.1);
        Ok(Session { vt, format, sink, target })
    }

    /// Moves pictures the callback produced into the reorder window; reports a decode error.
    fn collect(&mut self) -> Result<()> {
        let Some(s) = &self.session else { return Ok(()) };
        if let Some(e) = s.sink.error.lock().unwrap().take() {
            return Err(Error::Decode(e));
        }
        let mut frames = std::mem::take(&mut *s.sink.frames.lock().unwrap());
        frames.sort_by_key(|f| f.pts);
        self.window.extend(frames);
        Ok(())
    }

    /// Lets the session emit what it holds back, then collects it.
    fn finish(&mut self) -> Result<()> {
        if let Some(s) = &self.session {
            // SAFETY: a live session.
            unsafe {
                s.vt.finish_delayed_frames();
                s.vt.wait_for_asynchronous_frames();
            }
        }
        self.collect()
    }
}

impl VideoDecoder for VtVideoDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        self.eof = false;
        if packet.keyframe {
            // A new output size takes effect at a keyframe: a session for the new size (changing
            // a running session's size mid-GOP is what corrupted NVDEC's pictures).
            let retarget = self.session.as_ref().is_some_and(|s| s.target != target_size(self.display(), self.hint));
            if retarget {
                self.finish()?;
                self.session = None;
            }
            if self.session.is_none() {
                self.session = Some(self.create_session(&packet.data)?);
            }
        }
        let Some(session) = &self.session else {
            return Ok(()); // nothing decodable before the first keyframe
        };
        let sample = sample_buffer(&session.format, &packet.data, packet.pts)?;
        // SAFETY: a live session and sample buffer; no per-frame context.
        let status = unsafe {
            session.vt.decode_frame(&sample, VTDecodeFrameFlags::Frame_EnableTemporalProcessing, ptr::null_mut(), ptr::null_mut())
        };
        if status != 0 {
            return Err(Error::Decode(format!("VideoToolbox decode: OSStatus {status}")));
        }
        self.collect()
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        self.collect()?;
        if self.window.is_empty() {
            return Ok(None);
        }
        Ok(Some(DecodedFrame::Yuv(self.window.remove(0))))
    }

    fn flush(&mut self) {
        self.session = None; // the next keyframe starts a new one
        self.window.clear();
        self.eof = false;
    }

    fn send_eof(&mut self) {
        if let Err(e) = self.finish() {
            log::warn!("VideoToolbox at end of stream: {e}");
        }
        self.eof = true;
    }

    fn set_output_hint(&mut self, max: Option<(u32, u32)>) {
        self.hint = max;
    }
}

/// A one-sample `CMSampleBuffer` holding a copy of `data`, presented at `pts`.
fn sample_buffer(format: &CMVideoFormatDescription, data: &[u8], pts: Duration) -> Result<CFRetained<CMSampleBuffer>> {
    let len = data.len();
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: CoreMedia allocates `len` bytes (null memory block); the out-pointer receives an
    // owned buffer, which is then filled from `data`.
    let block = unsafe {
        let status = CMBlockBuffer::create_with_memory_block(None, ptr::null_mut(), len, None, ptr::null(), 0, len, 0, NonNull::from(&mut block));
        let block = cf::created(status, block, "CMBlockBufferCreateWithMemoryBlock")?;
        let status = CMBlockBuffer::replace_data_bytes(NonNull::new_unchecked(data.as_ptr() as *mut c_void), &block, 0, len);
        if status != 0 {
            return Err(Error::Decode(format!("CMBlockBufferReplaceDataBytes: OSStatus {status}")));
        }
        block
    };
    // SAFETY: reading an immutable framework constant.
    let invalid = unsafe { kCMTimeInvalid };
    let timing = CMSampleTimingInfo {
        duration: invalid,
        // SAFETY: plain value constructor.
        presentationTimeStamp: unsafe { CMTime::new(pts.as_nanos().min(i64::MAX as u128) as i64, TIMESCALE) },
        decodeTimeStamp: invalid,
    };
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    // SAFETY: one sample of `len` bytes with one timing entry; owned out-pointer.
    unsafe {
        let status = CMSampleBuffer::create_ready(None, Some(&block), Some(format), 1, 1, &timing, 1, &len, NonNull::from(&mut sample));
        cf::created(status, sample, "CMSampleBufferCreateReady")
    }
}

/// VideoToolbox's output callback: copies the picture out (or records the error).
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` is the session's `Sink`, alive until the session is invalidated.
    let sink = unsafe { &*(refcon as *const Sink) };
    if status != 0 {
        *sink.error.lock().unwrap() = Some(format!("VideoToolbox decode: OSStatus {status}"));
        return;
    }
    if image.is_null() || flags.contains(VTDecodeInfoFlags::FrameDropped) {
        return;
    }
    // SAFETY: a valid pixel buffer for the duration of the callback.
    match unsafe { copy_out(&*image, pts, sink) } {
        Ok(f) => sink.frames.lock().unwrap().push(f),
        Err(e) => *sink.error.lock().unwrap() = Some(e),
    }
}

/// The picture in `pb` (4:2:0 bi-planar, 8 or 10 bits) as an I420 frame.
///
/// # Safety
/// `pb` must be a valid pixel buffer.
unsafe fn copy_out(pb: &CVImageBuffer, pts: CMTime, sink: &Sink) -> std::result::Result<YuvFrame, String> {
    let code = CVPixelBufferGetPixelFormatType(pb);
    let Some(format) = FORMATS.iter().find(|f| f.code == code) else {
        return Err(format!("VideoToolbox output format {:?} is not a bi-planar YCbCr one", code.to_be_bytes()));
    };
    let (bytes_per_sample, full_range, layout) = (if format.ten_bit { 2 } else { 1 }, format.full_range, format.layout);
    // SAFETY: a valid pixel buffer, locked for reading while its planes are read.
    unsafe {
        if CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return Err("could not lock a VideoToolbox picture".into());
        }
    }
    let (w, h) = (CVPixelBufferGetWidthOfPlane(pb, 0), CVPixelBufferGetHeightOfPlane(pb, 0));
    let (y_base, y_stride) = (CVPixelBufferGetBaseAddressOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 0));
    let (uv_base, uv_stride) = (CVPixelBufferGetBaseAddressOfPlane(pb, 1), CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
    let uv_rows = CVPixelBufferGetHeightOfPlane(pb, 1);
    let planes = if y_base.is_null() || uv_base.is_null() {
        None
    } else {
        // SAFETY: the locked planes span `stride × rows` bytes each.
        let (y, uv) = unsafe {
            (
                std::slice::from_raw_parts(y_base as *const u8, y_stride * h),
                std::slice::from_raw_parts(uv_base as *const u8, uv_stride * uv_rows),
            )
        };
        Some(biplanar_to_planar(y, y_stride, uv, uv_stride, w as u32, h as u32, bytes_per_sample, layout))
    };
    // SAFETY: unlocks the lock taken above.
    unsafe { CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) };
    let planes = planes.ok_or("a VideoToolbox picture without planes")?;
    let (w, h) = (w as u32, h as u32);
    let (cw, _) = chroma_size(layout, w, h);
    let pts = if pts.timescale > 0 && pts.value >= 0 {
        Duration::from_nanos((pts.value as i128 * 1_000_000_000 / pts.timescale as i128) as u64)
    } else {
        Duration::ZERO
    };
    Ok(YuvFrame {
        width: w,
        height: h,
        layout,
        planes,
        strides: [w as usize, cw as usize, cw as usize],
        matrix: sink.matrix.unwrap_or_else(|| ColorMatrix::guess_for_height(sink.display_height)),
        full_range,
        pts,
    })
}
