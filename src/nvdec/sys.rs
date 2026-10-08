//! The parts of NVIDIA's NVDEC (`cuviddec.h`, `nvcuvid.h`) and CUDA driver (`cuda.h`) APIs we
//! use, transcribed from NVIDIA's MIT-licensed headers and loaded at runtime: nothing is linked,
//! so building needs no NVIDIA SDK and the program runs on machines without NVIDIA drivers.
// Names follow NVIDIA's headers, so the C documentation applies as written.
#![allow(non_camel_case_types, non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::{c_int, c_short, c_uchar, c_uint, c_ulong, c_ulonglong, c_ushort, c_void};
use std::sync::OnceLock;

pub type CUresult = c_int;
pub const CUDA_SUCCESS: CUresult = 0;
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void;
pub type CUdeviceptr = c_ulonglong;
pub type CUvideoparser = *mut c_void;
pub type CUvideodecoder = *mut c_void;
pub type CUvideoctxlock = *mut c_void;
pub type CUvideotimestamp = i64;

pub type cudaVideoCodec = c_uint;
pub const CODEC_H264: cudaVideoCodec = 4;
pub const CODEC_HEVC: cudaVideoCodec = 8;
pub const CODEC_VP8: cudaVideoCodec = 9;
pub const CODEC_VP9: cudaVideoCodec = 10;
pub const CODEC_AV1: cudaVideoCodec = 11;
pub const CHROMA_420: c_uint = 1;
pub const SURFACE_NV12: c_uint = 0;
pub const SURFACE_P016: c_uint = 1;
pub const DEINTERLACE_WEAVE: c_uint = 0;
pub const DEINTERLACE_ADAPTIVE: c_uint = 2;
pub const CREATE_PREFER_CUVID: c_ulong = 4;
pub const CUVID_PKT_ENDOFSTREAM: c_ulong = 1;
pub const CUVID_PKT_TIMESTAMP: c_ulong = 2;
pub const CU_MEMORYTYPE_HOST: c_uint = 1;
pub const CU_MEMORYTYPE_DEVICE: c_uint = 2;

/// Picture parameters: produced by the parser, passed to the decoder untouched.
pub enum CUVIDPICPARAMS {}

/// Plain C structs for which all-zero bytes are a valid value (null pointers, `None` callbacks).
pub trait Zeroable {}

/// An all-zero `T`, the C idiom `memset(&s, 0, sizeof s)` for the structs below.
pub fn zeroed<T: Zeroable>() -> T {
    // SAFETY: `Zeroable` is only implemented for the `repr(C)` structs in this module, made of
    // integers, raw pointers and `Option<fn>`, for which zero bytes are valid.
    unsafe { std::mem::zeroed() }
}

macro_rules! zeroable {
    ($($t:ty),*) => { $(impl Zeroable for $t {})* };
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect16 {
    pub left: c_short,
    pub top: c_short,
    pub right: c_short,
    pub bottom: c_short,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect32 {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Fraction {
    pub numerator: c_uint,
    pub denominator: c_uint,
}

/// `video_signal_description`: a bitfield byte (video_format:3, video_full_range_flag:1,
/// reserved:4), then the H.273 colour description.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoSignal {
    pub flags: c_uchar,
    pub color_primaries: c_uchar,
    pub transfer_characteristics: c_uchar,
    pub matrix_coefficients: c_uchar,
}

impl VideoSignal {
    pub fn full_range(&self) -> bool {
        self.flags & 0b1000 != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDEOFORMAT {
    pub codec: cudaVideoCodec,
    pub frame_rate: Fraction,
    pub progressive_sequence: c_uchar,
    pub bit_depth_luma_minus8: c_uchar,
    pub bit_depth_chroma_minus8: c_uchar,
    pub min_num_decode_surfaces: c_uchar,
    pub coded_width: c_uint,
    pub coded_height: c_uint,
    pub display_area: Rect32,
    pub chroma_format: c_uint,
    pub bitrate: c_uint,
    pub display_aspect_ratio: [c_int; 2],
    pub video_signal_description: VideoSignal,
    pub seqhdr_data_length: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDDECODECAPS {
    pub eCodecType: cudaVideoCodec,
    pub eChromaFormat: c_uint,
    pub nBitDepthMinus8: c_uint,
    pub reserved1: [c_uint; 3],
    pub bIsSupported: c_uchar,
    pub nNumNVDECs: c_uchar,
    pub nOutputFormatMask: c_ushort,
    pub nMaxWidth: c_uint,
    pub nMaxHeight: c_uint,
    pub nMaxMBCount: c_uint,
    pub nMinWidth: c_ushort,
    pub nMinHeight: c_ushort,
    pub bIsHistogramSupported: c_uchar,
    pub nCounterBitDepth: c_uchar,
    pub nMaxHistogramBins: c_ushort,
    pub reserved3: [c_uint; 10],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDDECODECREATEINFO {
    pub ulWidth: c_ulong,
    pub ulHeight: c_ulong,
    pub ulNumDecodeSurfaces: c_ulong,
    pub CodecType: cudaVideoCodec,
    pub ChromaFormat: c_uint,
    pub ulCreationFlags: c_ulong,
    pub bitDepthMinus8: c_ulong,
    pub ulIntraDecodeOnly: c_ulong,
    pub ulMaxWidth: c_ulong,
    pub ulMaxHeight: c_ulong,
    pub Reserved1: c_ulong,
    pub display_area: Rect16,
    pub OutputFormat: c_uint,
    pub DeinterlaceMode: c_uint,
    pub ulTargetWidth: c_ulong,
    pub ulTargetHeight: c_ulong,
    pub ulNumOutputSurfaces: c_ulong,
    pub vidLock: CUvideoctxlock,
    pub target_rect: Rect16,
    pub enableHistogram: c_ulong,
    pub Reserved2: [c_ulong; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDRECONFIGUREDECODERINFO {
    pub ulWidth: c_uint,
    pub ulHeight: c_uint,
    pub ulTargetWidth: c_uint,
    pub ulTargetHeight: c_uint,
    pub ulNumDecodeSurfaces: c_uint,
    pub reserved1: [c_uint; 12],
    pub display_area: Rect16,
    pub target_rect: Rect16,
    pub reserved2: [c_uint; 11],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDPROCPARAMS {
    pub progressive_frame: c_int,
    pub second_field: c_int,
    pub top_field_first: c_int,
    pub unpaired_field: c_int,
    pub reserved_flags: c_uint,
    pub reserved_zero: c_uint,
    pub raw_input_dptr: c_ulonglong,
    pub raw_input_pitch: c_uint,
    pub raw_input_format: c_uint,
    pub raw_output_dptr: c_ulonglong,
    pub raw_output_pitch: c_uint,
    pub Reserved1: c_uint,
    pub output_stream: *mut c_void,
    pub Reserved: [c_uint; 46],
    pub histogram_dptr: *mut c_ulonglong,
    pub Reserved2: [*mut c_void; 1],
}

pub type SequenceCallback = unsafe extern "C" fn(*mut c_void, *mut CUVIDEOFORMAT) -> c_int;
pub type DecodeCallback = unsafe extern "C" fn(*mut c_void, *mut CUVIDPICPARAMS) -> c_int;
pub type DisplayCallback = unsafe extern "C" fn(*mut c_void, *mut CUVIDPARSERDISPINFO) -> c_int;
/// Operating-point and SEI callbacks (unused; their struct arguments stay opaque).
pub type OtherCallback = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDPARSERPARAMS {
    pub CodecType: cudaVideoCodec,
    pub ulMaxNumDecodeSurfaces: c_uint,
    pub ulClockRate: c_uint,
    pub ulErrorThreshold: c_uint,
    pub ulMaxDisplayDelay: c_uint,
    /// Bitfield: bAnnexb:1 (AV1 Annex B input), reserved:31.
    pub flags: c_uint,
    pub uReserved1: [c_uint; 4],
    pub pUserData: *mut c_void,
    pub pfnSequenceCallback: Option<SequenceCallback>,
    pub pfnDecodePicture: Option<DecodeCallback>,
    pub pfnDisplayPicture: Option<DisplayCallback>,
    pub pfnGetOperatingPoint: Option<OtherCallback>,
    pub pfnGetSEIMsg: Option<OtherCallback>,
    pub pvReserved2: [*mut c_void; 5],
    pub pExtVideoInfo: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDSOURCEDATAPACKET {
    pub flags: c_ulong,
    pub payload_size: c_ulong,
    pub payload: *const c_uchar,
    pub timestamp: CUvideotimestamp,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUVIDPARSERDISPINFO {
    pub picture_index: c_int,
    pub progressive_frame: c_int,
    pub top_field_first: c_int,
    pub repeat_first_field: c_int,
    pub timestamp: CUvideotimestamp,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CUDA_MEMCPY2D {
    pub srcXInBytes: usize,
    pub srcY: usize,
    pub srcMemoryType: c_uint,
    pub srcHost: *const c_void,
    pub srcDevice: CUdeviceptr,
    pub srcArray: *mut c_void,
    pub srcPitch: usize,
    pub dstXInBytes: usize,
    pub dstY: usize,
    pub dstMemoryType: c_uint,
    pub dstHost: *mut c_void,
    pub dstDevice: CUdeviceptr,
    pub dstArray: *mut c_void,
    pub dstPitch: usize,
    pub WidthInBytes: usize,
    pub Height: usize,
}

zeroable!(
    Rect16, Rect32, Fraction, VideoSignal, CUVIDEOFORMAT, CUVIDDECODECAPS, CUVIDDECODECREATEINFO,
    CUVIDRECONFIGUREDECODERINFO, CUVIDPROCPARAMS, CUVIDPARSERPARAMS, CUVIDSOURCEDATAPACKET,
    CUVIDPARSERDISPINFO, CUDA_MEMCPY2D
);

/// The functions we call, resolved once from the driver's libraries.
pub struct Api {
    pub cuInit: unsafe extern "C" fn(c_uint) -> CUresult,
    pub cuDeviceGet: unsafe extern "C" fn(*mut CUdevice, c_int) -> CUresult,
    pub cuCtxCreate: unsafe extern "C" fn(*mut CUcontext, c_uint, CUdevice) -> CUresult,
    pub cuCtxPushCurrent: unsafe extern "C" fn(CUcontext) -> CUresult,
    pub cuCtxPopCurrent: unsafe extern "C" fn(*mut CUcontext) -> CUresult,
    pub cuMemAllocHost: unsafe extern "C" fn(*mut *mut c_void, usize) -> CUresult,
    pub cuMemFreeHost: unsafe extern "C" fn(*mut c_void) -> CUresult,
    pub cuMemcpy2D: unsafe extern "C" fn(*const CUDA_MEMCPY2D) -> CUresult,
    pub cuvidGetDecoderCaps: unsafe extern "C" fn(*mut CUVIDDECODECAPS) -> CUresult,
    pub cuvidCreateVideoParser: unsafe extern "C" fn(*mut CUvideoparser, *mut CUVIDPARSERPARAMS) -> CUresult,
    pub cuvidParseVideoData: unsafe extern "C" fn(CUvideoparser, *mut CUVIDSOURCEDATAPACKET) -> CUresult,
    pub cuvidDestroyVideoParser: unsafe extern "C" fn(CUvideoparser) -> CUresult,
    pub cuvidCreateDecoder: unsafe extern "C" fn(*mut CUvideodecoder, *mut CUVIDDECODECREATEINFO) -> CUresult,
    pub cuvidReconfigureDecoder: unsafe extern "C" fn(CUvideodecoder, *mut CUVIDRECONFIGUREDECODERINFO) -> CUresult,
    pub cuvidDecodePicture: unsafe extern "C" fn(CUvideodecoder, *mut CUVIDPICPARAMS) -> CUresult,
    pub cuvidMapVideoFrame64:
        unsafe extern "C" fn(CUvideodecoder, c_int, *mut c_ulonglong, *mut c_uint, *mut CUVIDPROCPARAMS) -> CUresult,
    pub cuvidUnmapVideoFrame64: unsafe extern "C" fn(CUvideodecoder, c_ulonglong) -> CUresult,
    pub cuvidDestroyDecoder: unsafe extern "C" fn(CUvideodecoder) -> CUresult,
    /// Keeps the libraries loaded for as long as the function pointers above exist (forever).
    _libs: (libloading::Library, libloading::Library),
}

impl Api {
    /// The loaded API, or `None` without NVIDIA's driver. Loaded once per process.
    pub fn get() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| {
            // SAFETY: loading the driver's own libraries by soname; their initialisers are the
            // driver's, as when any CUDA program starts.
            unsafe { Api::load() }.map_err(|e| log::debug!("NVDEC unavailable: {e}")).ok()
        })
        .as_ref()
    }

    /// # Safety
    /// Loads native libraries and trusts the symbol types above (taken from NVIDIA's headers).
    unsafe fn load() -> Result<Api, libloading::Error> {
        // SAFETY: as documented on this function.
        unsafe {
            let cuda = libloading::Library::new("libcuda.so.1")?;
            let cuvid = libloading::Library::new("libnvcuvid.so.1")?;
            macro_rules! sym {
                ($lib:ident, $name:literal) => {
                    *$lib.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(Api {
                cuInit: sym!(cuda, "cuInit"),
                cuDeviceGet: sym!(cuda, "cuDeviceGet"),
                cuCtxCreate: sym!(cuda, "cuCtxCreate_v2"),
                cuCtxPushCurrent: sym!(cuda, "cuCtxPushCurrent_v2"),
                cuCtxPopCurrent: sym!(cuda, "cuCtxPopCurrent_v2"),
                cuMemAllocHost: sym!(cuda, "cuMemAllocHost_v2"),
                cuMemFreeHost: sym!(cuda, "cuMemFreeHost"),
                cuMemcpy2D: sym!(cuda, "cuMemcpy2D_v2"),
                cuvidGetDecoderCaps: sym!(cuvid, "cuvidGetDecoderCaps"),
                cuvidCreateVideoParser: sym!(cuvid, "cuvidCreateVideoParser"),
                cuvidParseVideoData: sym!(cuvid, "cuvidParseVideoData"),
                cuvidDestroyVideoParser: sym!(cuvid, "cuvidDestroyVideoParser"),
                cuvidCreateDecoder: sym!(cuvid, "cuvidCreateDecoder"),
                cuvidReconfigureDecoder: sym!(cuvid, "cuvidReconfigureDecoder"),
                cuvidDecodePicture: sym!(cuvid, "cuvidDecodePicture"),
                cuvidMapVideoFrame64: sym!(cuvid, "cuvidMapVideoFrame64"),
                cuvidUnmapVideoFrame64: sym!(cuvid, "cuvidUnmapVideoFrame64"),
                cuvidDestroyDecoder: sym!(cuvid, "cuvidDestroyDecoder"),
                _libs: (cuda, cuvid),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets from NVIDIA's headers (x86-64 / aarch64 Linux, LP64).
    #[test]
    fn layouts_match_nvidias_headers() {
        assert_eq!(size_of::<CUVIDDECODECAPS>(), 88);
        assert_eq!(offset_of!(CUVIDDECODECAPS, bIsSupported), 24);
        assert_eq!(offset_of!(CUVIDDECODECAPS, nMaxWidth), 28);
        assert_eq!(offset_of!(CUVIDDECODECAPS, nMinWidth), 40);
        assert_eq!(size_of::<CUVIDDECODECREATEINFO>(), 176);
        assert_eq!(offset_of!(CUVIDDECODECREATEINFO, CodecType), 24);
        assert_eq!(offset_of!(CUVIDDECODECREATEINFO, display_area), 80);
        assert_eq!(offset_of!(CUVIDDECODECREATEINFO, OutputFormat), 88);
        assert_eq!(offset_of!(CUVIDDECODECREATEINFO, ulTargetWidth), 96);
        assert_eq!(offset_of!(CUVIDDECODECREATEINFO, vidLock), 120);
        assert_eq!(offset_of!(CUVIDDECODECREATEINFO, target_rect), 128);
        assert_eq!(size_of::<CUVIDRECONFIGUREDECODERINFO>(), 128);
        assert_eq!(offset_of!(CUVIDRECONFIGUREDECODERINFO, display_area), 68);
        assert_eq!(offset_of!(CUVIDRECONFIGUREDECODERINFO, target_rect), 76);
        assert_eq!(size_of::<CUVIDPROCPARAMS>(), 264);
        assert_eq!(offset_of!(CUVIDPROCPARAMS, output_stream), 56);
        assert_eq!(size_of::<CUVIDEOFORMAT>(), 64);
        assert_eq!(offset_of!(CUVIDEOFORMAT, coded_width), 16);
        assert_eq!(offset_of!(CUVIDEOFORMAT, display_area), 24);
        assert_eq!(offset_of!(CUVIDEOFORMAT, chroma_format), 40);
        assert_eq!(offset_of!(CUVIDEOFORMAT, video_signal_description), 56);
        assert_eq!(size_of::<CUVIDPARSERPARAMS>(), 136);
        assert_eq!(offset_of!(CUVIDPARSERPARAMS, pUserData), 40);
        assert_eq!(offset_of!(CUVIDPARSERPARAMS, pfnSequenceCallback), 48);
        assert_eq!(offset_of!(CUVIDPARSERPARAMS, pExtVideoInfo), 128);
        assert_eq!(size_of::<CUVIDSOURCEDATAPACKET>(), 32);
        assert_eq!(size_of::<CUVIDPARSERDISPINFO>(), 24);
        assert_eq!(offset_of!(CUVIDPARSERDISPINFO, timestamp), 16);
        assert_eq!(size_of::<CUDA_MEMCPY2D>(), 128);
        assert_eq!(offset_of!(CUDA_MEMCPY2D, srcDevice), 32);
        assert_eq!(offset_of!(CUDA_MEMCPY2D, dstHost), 80);
        assert_eq!(offset_of!(CUDA_MEMCPY2D, WidthInBytes), 112);
    }

    #[test]
    fn full_range_flag_is_bit_3_of_the_signal_byte() {
        let mut s: VideoSignal = zeroed();
        s.flags = 0b0000_1000;
        assert!(s.full_range());
        s.flags = 0b0000_0101; // video_format 5, limited range
        assert!(!s.full_range());
    }

    /// Runs on any Linux machine: loads when NVIDIA's driver is installed, else reports absence
    /// (never panics).
    #[test]
    fn loading_reports_presence_without_panicking() {
        eprintln!("NVDEC libraries present: {}", Api::get().is_some());
    }
}
