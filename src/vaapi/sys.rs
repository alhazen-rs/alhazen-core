//! libva's types (`va/va.h` 1.24, MIT-licensed), generated once with bindgen 0.72.1 and kept
//! here, and its functions loaded at runtime from `libva.so.2` and `libva-drm.so.2`: nothing is
//! linked, so the program runs where there's no libva, and building needs no headers. The
//! layout checks at the end compare every struct with the header's sizes and offsets.
// Names follow libva's header, so its documentation applies as written.
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, unused_imports, unnecessary_transmutes, clippy::all)]


#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct __BindgenBitfieldUnit<Storage> {
    storage: Storage,
}
impl<Storage> __BindgenBitfieldUnit<Storage> {
    #[inline]
    pub const fn new(storage: Storage) -> Self {
        Self { storage }
    }
}
impl<Storage> __BindgenBitfieldUnit<Storage>
where
    Storage: AsRef<[u8]> + AsMut<[u8]>,
{
    #[inline]
    fn extract_bit(byte: u8, index: usize) -> bool {
        let bit_index = if cfg!(target_endian = "big") {
            7 - (index % 8)
        } else {
            index % 8
        };
        let mask = 1 << bit_index;
        byte & mask == mask
    }
    #[inline]
    pub fn get_bit(&self, index: usize) -> bool {
        debug_assert!(index / 8 < self.storage.as_ref().len());
        let byte_index = index / 8;
        let byte = self.storage.as_ref()[byte_index];
        Self::extract_bit(byte, index)
    }
    #[inline]
    pub unsafe fn raw_get_bit(this: *const Self, index: usize) -> bool {
        debug_assert!(index / 8 < core::mem::size_of::<Storage>());
        let byte_index = index / 8;
        let byte = unsafe {
            *(core::ptr::addr_of!((*this).storage) as *const u8).offset(byte_index as isize)
        };
        Self::extract_bit(byte, index)
    }
    #[inline]
    fn change_bit(byte: u8, index: usize, val: bool) -> u8 {
        let bit_index = if cfg!(target_endian = "big") {
            7 - (index % 8)
        } else {
            index % 8
        };
        let mask = 1 << bit_index;
        if val {
            byte | mask
        } else {
            byte & !mask
        }
    }
    #[inline]
    pub fn set_bit(&mut self, index: usize, val: bool) {
        debug_assert!(index / 8 < self.storage.as_ref().len());
        let byte_index = index / 8;
        let byte = &mut self.storage.as_mut()[byte_index];
        *byte = Self::change_bit(*byte, index, val);
    }
    #[inline]
    pub unsafe fn raw_set_bit(this: *mut Self, index: usize, val: bool) {
        debug_assert!(index / 8 < core::mem::size_of::<Storage>());
        let byte_index = index / 8;
        let byte = unsafe {
            (core::ptr::addr_of_mut!((*this).storage) as *mut u8).offset(byte_index as isize)
        };
        unsafe { *byte = Self::change_bit(*byte, index, val) };
    }
    #[inline]
    pub fn get(&self, bit_offset: usize, bit_width: u8) -> u64 {
        debug_assert!(bit_width <= 64);
        debug_assert!(bit_offset / 8 < self.storage.as_ref().len());
        debug_assert!((bit_offset + (bit_width as usize)) / 8 <= self.storage.as_ref().len());
        let mut val = 0;
        for i in 0..(bit_width as usize) {
            if self.get_bit(i + bit_offset) {
                let index = if cfg!(target_endian = "big") {
                    bit_width as usize - 1 - i
                } else {
                    i
                };
                val |= 1 << index;
            }
        }
        val
    }
    #[inline]
    pub unsafe fn raw_get(this: *const Self, bit_offset: usize, bit_width: u8) -> u64 {
        debug_assert!(bit_width <= 64);
        debug_assert!(bit_offset / 8 < core::mem::size_of::<Storage>());
        debug_assert!((bit_offset + (bit_width as usize)) / 8 <= core::mem::size_of::<Storage>());
        let mut val = 0;
        for i in 0..(bit_width as usize) {
            if unsafe { Self::raw_get_bit(this, i + bit_offset) } {
                let index = if cfg!(target_endian = "big") {
                    bit_width as usize - 1 - i
                } else {
                    i
                };
                val |= 1 << index;
            }
        }
        val
    }
    #[inline]
    pub fn set(&mut self, bit_offset: usize, bit_width: u8, val: u64) {
        debug_assert!(bit_width <= 64);
        debug_assert!(bit_offset / 8 < self.storage.as_ref().len());
        debug_assert!((bit_offset + (bit_width as usize)) / 8 <= self.storage.as_ref().len());
        for i in 0..(bit_width as usize) {
            let mask = 1 << i;
            let val_bit_is_set = val & mask == mask;
            let index = if cfg!(target_endian = "big") {
                bit_width as usize - 1 - i
            } else {
                i
            };
            self.set_bit(index + bit_offset, val_bit_is_set);
        }
    }
    #[inline]
    pub unsafe fn raw_set(this: *mut Self, bit_offset: usize, bit_width: u8, val: u64) {
        debug_assert!(bit_width <= 64);
        debug_assert!(bit_offset / 8 < core::mem::size_of::<Storage>());
        debug_assert!((bit_offset + (bit_width as usize)) / 8 <= core::mem::size_of::<Storage>());
        for i in 0..(bit_width as usize) {
            let mask = 1 << i;
            let val_bit_is_set = val & mask == mask;
            let index = if cfg!(target_endian = "big") {
                bit_width as usize - 1 - i
            } else {
                i
            };
            unsafe { Self::raw_set_bit(this, index + bit_offset, val_bit_is_set) };
        }
    }
}
pub const VA_MAJOR_VERSION: u32 = 1;
pub const VA_MINOR_VERSION: u32 = 24;
pub const VA_STATUS_SUCCESS: u32 = 0;
pub const VA_RT_FORMAT_YUV420: u32 = 1;
pub const VA_INVALID_ID: u32 = 4294967295;
pub const VA_INVALID_SURFACE: u32 = 4294967295;
pub const VA_SURFACE_ATTRIB_SETTABLE: u32 = 2;
pub const VA_PROGRESSIVE: u32 = 1;
pub const VA_SLICE_DATA_FLAG_ALL: u32 = 0;
pub const VA_PICTURE_H264_INVALID: u32 = 1;
pub const VA_PICTURE_H264_TOP_FIELD: u32 = 2;
pub const VA_PICTURE_H264_BOTTOM_FIELD: u32 = 4;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 8;
pub const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 16;
pub const VA_PICTURE_H264_NON_EXISTING: u32 = 32;
pub const VA_FOURCC_NV12: u32 = 842094158;
pub type VADisplay = *mut ::core::ffi::c_void;
pub type VAStatus = ::core::ffi::c_int;
pub const VAProfileNone: VAProfile = -1;
pub const VAProfileMPEG2Simple: VAProfile = 0;
pub const VAProfileMPEG2Main: VAProfile = 1;
pub const VAProfileMPEG4Simple: VAProfile = 2;
pub const VAProfileMPEG4AdvancedSimple: VAProfile = 3;
pub const VAProfileMPEG4Main: VAProfile = 4;
pub const VAProfileH264Baseline: VAProfile = 5;
pub const VAProfileH264Main: VAProfile = 6;
pub const VAProfileH264High: VAProfile = 7;
pub const VAProfileVC1Simple: VAProfile = 8;
pub const VAProfileVC1Main: VAProfile = 9;
pub const VAProfileVC1Advanced: VAProfile = 10;
pub const VAProfileH263Baseline: VAProfile = 11;
pub const VAProfileJPEGBaseline: VAProfile = 12;
pub const VAProfileH264ConstrainedBaseline: VAProfile = 13;
pub const VAProfileVP8Version0_3: VAProfile = 14;
pub const VAProfileH264MultiviewHigh: VAProfile = 15;
pub const VAProfileH264StereoHigh: VAProfile = 16;
pub const VAProfileHEVCMain: VAProfile = 17;
pub const VAProfileHEVCMain10: VAProfile = 18;
pub const VAProfileVP9Profile0: VAProfile = 19;
pub const VAProfileVP9Profile1: VAProfile = 20;
pub const VAProfileVP9Profile2: VAProfile = 21;
pub const VAProfileVP9Profile3: VAProfile = 22;
pub const VAProfileHEVCMain12: VAProfile = 23;
pub const VAProfileHEVCMain422_10: VAProfile = 24;
pub const VAProfileHEVCMain422_12: VAProfile = 25;
pub const VAProfileHEVCMain444: VAProfile = 26;
pub const VAProfileHEVCMain444_10: VAProfile = 27;
pub const VAProfileHEVCMain444_12: VAProfile = 28;
pub const VAProfileHEVCSccMain: VAProfile = 29;
pub const VAProfileHEVCSccMain10: VAProfile = 30;
pub const VAProfileHEVCSccMain444: VAProfile = 31;
pub const VAProfileAV1Profile0: VAProfile = 32;
pub const VAProfileAV1Profile1: VAProfile = 33;
pub const VAProfileHEVCSccMain444_10: VAProfile = 34;
pub const VAProfileProtected: VAProfile = 35;
pub const VAProfileH264High10: VAProfile = 36;
pub const VAProfileVVCMain10: VAProfile = 37;
pub const VAProfileVVCMultilayerMain10: VAProfile = 38;
pub const VAProfileAV1Profile2: VAProfile = 39;
pub const VAProfileH264High422: VAProfile = 40;
pub type VAProfile = ::core::ffi::c_int;
pub const VAEntrypointVLD: VAEntrypoint = 1;
pub const VAEntrypointIZZ: VAEntrypoint = 2;
pub const VAEntrypointIDCT: VAEntrypoint = 3;
pub const VAEntrypointMoComp: VAEntrypoint = 4;
pub const VAEntrypointDeblocking: VAEntrypoint = 5;
pub const VAEntrypointEncSlice: VAEntrypoint = 6;
pub const VAEntrypointEncPicture: VAEntrypoint = 7;
pub const VAEntrypointEncSliceLP: VAEntrypoint = 8;
pub const VAEntrypointVideoProc: VAEntrypoint = 10;
pub const VAEntrypointFEI: VAEntrypoint = 11;
pub const VAEntrypointStats: VAEntrypoint = 12;
pub const VAEntrypointProtectedTEEComm: VAEntrypoint = 13;
pub const VAEntrypointProtectedContent: VAEntrypoint = 14;
pub type VAEntrypoint = ::core::ffi::c_uint;
pub const VAConfigAttribRTFormat: VAConfigAttribType = 0;
pub const VAConfigAttribSpatialResidual: VAConfigAttribType = 1;
pub const VAConfigAttribSpatialClipping: VAConfigAttribType = 2;
pub const VAConfigAttribIntraResidual: VAConfigAttribType = 3;
pub const VAConfigAttribEncryption: VAConfigAttribType = 4;
pub const VAConfigAttribRateControl: VAConfigAttribType = 5;
pub const VAConfigAttribDecSliceMode: VAConfigAttribType = 6;
pub const VAConfigAttribDecJPEG: VAConfigAttribType = 7;
pub const VAConfigAttribDecProcessing: VAConfigAttribType = 8;
pub const VAConfigAttribEncPackedHeaders: VAConfigAttribType = 10;
pub const VAConfigAttribEncInterlaced: VAConfigAttribType = 11;
pub const VAConfigAttribEncMaxRefFrames: VAConfigAttribType = 13;
pub const VAConfigAttribEncMaxSlices: VAConfigAttribType = 14;
pub const VAConfigAttribEncSliceStructure: VAConfigAttribType = 15;
pub const VAConfigAttribEncMacroblockInfo: VAConfigAttribType = 16;
pub const VAConfigAttribMaxPictureWidth: VAConfigAttribType = 18;
pub const VAConfigAttribMaxPictureHeight: VAConfigAttribType = 19;
pub const VAConfigAttribEncJPEG: VAConfigAttribType = 20;
pub const VAConfigAttribEncQualityRange: VAConfigAttribType = 21;
pub const VAConfigAttribEncQuantization: VAConfigAttribType = 22;
pub const VAConfigAttribEncIntraRefresh: VAConfigAttribType = 23;
pub const VAConfigAttribEncSkipFrame: VAConfigAttribType = 24;
pub const VAConfigAttribEncROI: VAConfigAttribType = 25;
pub const VAConfigAttribEncRateControlExt: VAConfigAttribType = 26;
pub const VAConfigAttribProcessingRate: VAConfigAttribType = 27;
pub const VAConfigAttribEncDirtyRect: VAConfigAttribType = 28;
pub const VAConfigAttribEncParallelRateControl: VAConfigAttribType = 29;
pub const VAConfigAttribEncDynamicScaling: VAConfigAttribType = 30;
pub const VAConfigAttribFrameSizeToleranceSupport: VAConfigAttribType = 31;
pub const VAConfigAttribFEIFunctionType: VAConfigAttribType = 32;
pub const VAConfigAttribFEIMVPredictors: VAConfigAttribType = 33;
pub const VAConfigAttribStats: VAConfigAttribType = 34;
pub const VAConfigAttribEncTileSupport: VAConfigAttribType = 35;
pub const VAConfigAttribCustomRoundingControl: VAConfigAttribType = 36;
pub const VAConfigAttribQPBlockSize: VAConfigAttribType = 37;
pub const VAConfigAttribMaxFrameSize: VAConfigAttribType = 38;
pub const VAConfigAttribPredictionDirection: VAConfigAttribType = 39;
pub const VAConfigAttribMultipleFrame: VAConfigAttribType = 40;
pub const VAConfigAttribContextPriority: VAConfigAttribType = 41;
pub const VAConfigAttribDecAV1Features: VAConfigAttribType = 42;
pub const VAConfigAttribTEEType: VAConfigAttribType = 43;
pub const VAConfigAttribTEETypeClient: VAConfigAttribType = 44;
pub const VAConfigAttribProtectedContentCipherAlgorithm: VAConfigAttribType = 45;
pub const VAConfigAttribProtectedContentCipherBlockSize: VAConfigAttribType = 46;
pub const VAConfigAttribProtectedContentCipherMode: VAConfigAttribType = 47;
pub const VAConfigAttribProtectedContentCipherSampleType: VAConfigAttribType = 48;
pub const VAConfigAttribProtectedContentUsage: VAConfigAttribType = 49;
pub const VAConfigAttribEncHEVCFeatures: VAConfigAttribType = 50;
pub const VAConfigAttribEncHEVCBlockSizes: VAConfigAttribType = 51;
pub const VAConfigAttribEncAV1: VAConfigAttribType = 52;
pub const VAConfigAttribEncAV1Ext1: VAConfigAttribType = 53;
pub const VAConfigAttribEncAV1Ext2: VAConfigAttribType = 54;
pub const VAConfigAttribEncPerBlockControl: VAConfigAttribType = 55;
pub const VAConfigAttribEncMaxTileRows: VAConfigAttribType = 56;
pub const VAConfigAttribEncMaxTileCols: VAConfigAttribType = 57;
pub const VAConfigAttribEncVP9: VAConfigAttribType = 58;
pub const VAConfigAttribTypeMax: VAConfigAttribType = 59;
pub type VAConfigAttribType = ::core::ffi::c_uint;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _VAConfigAttrib {
    pub type_: VAConfigAttribType,
    pub value: u32,
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAConfigAttrib"][::core::mem::size_of::<_VAConfigAttrib>() - 8usize];
    ["Alignment of _VAConfigAttrib"][::core::mem::align_of::<_VAConfigAttrib>() - 4usize];
    ["Offset of field: _VAConfigAttrib::type_"]
        [::core::mem::offset_of!(_VAConfigAttrib, type_) - 0usize];
    ["Offset of field: _VAConfigAttrib::value"]
        [::core::mem::offset_of!(_VAConfigAttrib, value) - 4usize];
};
pub type VAConfigAttrib = _VAConfigAttrib;
pub type VAGenericID = ::core::ffi::c_uint;
pub type VAConfigID = VAGenericID;
pub type VAContextID = VAGenericID;
pub type VASurfaceID = VAGenericID;
pub const VAGenericValueTypeInteger: VAGenericValueType = 1;
pub const VAGenericValueTypeFloat: VAGenericValueType = 2;
pub const VAGenericValueTypePointer: VAGenericValueType = 3;
pub const VAGenericValueTypeFunc: VAGenericValueType = 4;
pub type VAGenericValueType = ::core::ffi::c_uint;
pub type VAGenericFunc = ::core::option::Option<unsafe extern "C" fn()>;
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _VAGenericValue {
    pub type_: VAGenericValueType,
    pub value: _VAGenericValue__bindgen_ty_1,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union _VAGenericValue__bindgen_ty_1 {
    pub i: i32,
    pub f: f32,
    pub p: *mut ::core::ffi::c_void,
    pub fn_: VAGenericFunc,
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAGenericValue__bindgen_ty_1"]
        [::core::mem::size_of::<_VAGenericValue__bindgen_ty_1>() - 8usize];
    ["Alignment of _VAGenericValue__bindgen_ty_1"]
        [::core::mem::align_of::<_VAGenericValue__bindgen_ty_1>() - 8usize];
    ["Offset of field: _VAGenericValue__bindgen_ty_1::i"]
        [::core::mem::offset_of!(_VAGenericValue__bindgen_ty_1, i) - 0usize];
    ["Offset of field: _VAGenericValue__bindgen_ty_1::f"]
        [::core::mem::offset_of!(_VAGenericValue__bindgen_ty_1, f) - 0usize];
    ["Offset of field: _VAGenericValue__bindgen_ty_1::p"]
        [::core::mem::offset_of!(_VAGenericValue__bindgen_ty_1, p) - 0usize];
    ["Offset of field: _VAGenericValue__bindgen_ty_1::fn_"]
        [::core::mem::offset_of!(_VAGenericValue__bindgen_ty_1, fn_) - 0usize];
};
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAGenericValue"][::core::mem::size_of::<_VAGenericValue>() - 16usize];
    ["Alignment of _VAGenericValue"][::core::mem::align_of::<_VAGenericValue>() - 8usize];
    ["Offset of field: _VAGenericValue::type_"]
        [::core::mem::offset_of!(_VAGenericValue, type_) - 0usize];
    ["Offset of field: _VAGenericValue::value"]
        [::core::mem::offset_of!(_VAGenericValue, value) - 8usize];
};
pub type VAGenericValue = _VAGenericValue;
pub const VASurfaceAttribNone: VASurfaceAttribType = 0;
pub const VASurfaceAttribPixelFormat: VASurfaceAttribType = 1;
pub const VASurfaceAttribMinWidth: VASurfaceAttribType = 2;
pub const VASurfaceAttribMaxWidth: VASurfaceAttribType = 3;
pub const VASurfaceAttribMinHeight: VASurfaceAttribType = 4;
pub const VASurfaceAttribMaxHeight: VASurfaceAttribType = 5;
pub const VASurfaceAttribMemoryType: VASurfaceAttribType = 6;
pub const VASurfaceAttribExternalBufferDescriptor: VASurfaceAttribType = 7;
pub const VASurfaceAttribUsageHint: VASurfaceAttribType = 8;
pub const VASurfaceAttribDRMFormatModifiers: VASurfaceAttribType = 9;
pub const VASurfaceAttribAlignmentSize: VASurfaceAttribType = 10;
pub const VASurfaceAttribCount: VASurfaceAttribType = 11;
pub type VASurfaceAttribType = ::core::ffi::c_uint;
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _VASurfaceAttrib {
    pub type_: VASurfaceAttribType,
    pub flags: u32,
    pub value: VAGenericValue,
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VASurfaceAttrib"][::core::mem::size_of::<_VASurfaceAttrib>() - 24usize];
    ["Alignment of _VASurfaceAttrib"][::core::mem::align_of::<_VASurfaceAttrib>() - 8usize];
    ["Offset of field: _VASurfaceAttrib::type_"]
        [::core::mem::offset_of!(_VASurfaceAttrib, type_) - 0usize];
    ["Offset of field: _VASurfaceAttrib::flags"]
        [::core::mem::offset_of!(_VASurfaceAttrib, flags) - 4usize];
    ["Offset of field: _VASurfaceAttrib::value"]
        [::core::mem::offset_of!(_VASurfaceAttrib, value) - 8usize];
};
pub type VASurfaceAttrib = _VASurfaceAttrib;
pub type VABufferID = VAGenericID;
pub const VAPictureParameterBufferType: VABufferType = 0;
pub const VAIQMatrixBufferType: VABufferType = 1;
pub const VABitPlaneBufferType: VABufferType = 2;
pub const VASliceGroupMapBufferType: VABufferType = 3;
pub const VASliceParameterBufferType: VABufferType = 4;
pub const VASliceDataBufferType: VABufferType = 5;
pub const VAMacroblockParameterBufferType: VABufferType = 6;
pub const VAResidualDataBufferType: VABufferType = 7;
pub const VADeblockingParameterBufferType: VABufferType = 8;
pub const VAImageBufferType: VABufferType = 9;
pub const VAProtectedSliceDataBufferType: VABufferType = 10;
pub const VAQMatrixBufferType: VABufferType = 11;
pub const VAHuffmanTableBufferType: VABufferType = 12;
pub const VAProbabilityBufferType: VABufferType = 13;
pub const VAEncCodedBufferType: VABufferType = 21;
pub const VAEncSequenceParameterBufferType: VABufferType = 22;
pub const VAEncPictureParameterBufferType: VABufferType = 23;
pub const VAEncSliceParameterBufferType: VABufferType = 24;
pub const VAEncPackedHeaderParameterBufferType: VABufferType = 25;
pub const VAEncPackedHeaderDataBufferType: VABufferType = 26;
pub const VAEncMiscParameterBufferType: VABufferType = 27;
pub const VAEncMacroblockParameterBufferType: VABufferType = 28;
pub const VAEncMacroblockMapBufferType: VABufferType = 29;
pub const VAEncQPBufferType: VABufferType = 30;
pub const VAProcPipelineParameterBufferType: VABufferType = 41;
pub const VAProcFilterParameterBufferType: VABufferType = 42;
pub const VAEncFEIMVBufferType: VABufferType = 43;
pub const VAEncFEIMBCodeBufferType: VABufferType = 44;
pub const VAEncFEIDistortionBufferType: VABufferType = 45;
pub const VAEncFEIMBControlBufferType: VABufferType = 46;
pub const VAEncFEIMVPredictorBufferType: VABufferType = 47;
pub const VAStatsStatisticsParameterBufferType: VABufferType = 48;
pub const VAStatsStatisticsBufferType: VABufferType = 49;
pub const VAStatsStatisticsBottomFieldBufferType: VABufferType = 50;
pub const VAStatsMVBufferType: VABufferType = 51;
pub const VAStatsMVPredictorBufferType: VABufferType = 52;
pub const VAEncMacroblockDisableSkipMapBufferType: VABufferType = 53;
pub const VAEncFEICTBCmdBufferType: VABufferType = 54;
pub const VAEncFEICURecordBufferType: VABufferType = 55;
pub const VADecodeStreamoutBufferType: VABufferType = 56;
pub const VASubsetsParameterBufferType: VABufferType = 57;
pub const VAContextParameterUpdateBufferType: VABufferType = 58;
pub const VAProtectedSessionExecuteBufferType: VABufferType = 59;
pub const VAEncryptionParameterBufferType: VABufferType = 60;
pub const VAEncDeltaQpPerBlockBufferType: VABufferType = 61;
pub const VAAlfBufferType: VABufferType = 62;
pub const VALmcsBufferType: VABufferType = 63;
pub const VASubPicBufferType: VABufferType = 64;
pub const VATileBufferType: VABufferType = 65;
pub const VASliceStructBufferType: VABufferType = 66;
pub const VABufferTypeMax: VABufferType = 67;
pub type VABufferType = ::core::ffi::c_uint;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; 4usize],
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAPictureH264"][::core::mem::size_of::<_VAPictureH264>() - 36usize];
    ["Alignment of _VAPictureH264"][::core::mem::align_of::<_VAPictureH264>() - 4usize];
    ["Offset of field: _VAPictureH264::picture_id"]
        [::core::mem::offset_of!(_VAPictureH264, picture_id) - 0usize];
    ["Offset of field: _VAPictureH264::frame_idx"]
        [::core::mem::offset_of!(_VAPictureH264, frame_idx) - 4usize];
    ["Offset of field: _VAPictureH264::flags"]
        [::core::mem::offset_of!(_VAPictureH264, flags) - 8usize];
    ["Offset of field: _VAPictureH264::TopFieldOrderCnt"]
        [::core::mem::offset_of!(_VAPictureH264, TopFieldOrderCnt) - 12usize];
    ["Offset of field: _VAPictureH264::BottomFieldOrderCnt"]
        [::core::mem::offset_of!(_VAPictureH264, BottomFieldOrderCnt) - 16usize];
    ["Offset of field: _VAPictureH264::va_reserved"]
        [::core::mem::offset_of!(_VAPictureH264, va_reserved) - 20usize];
};
pub type VAPictureH264 = _VAPictureH264;
#[repr(C)]
#[derive(Copy, Clone)]
pub struct _VAPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16usize],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    pub seq_fields: _VAPictureParameterBufferH264__bindgen_ty_1,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: _VAPictureParameterBufferH264__bindgen_ty_2,
    pub frame_num: u16,
    pub va_reserved: [u32; 8usize],
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union _VAPictureParameterBufferH264__bindgen_ty_1 {
    pub bits: _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1,
    pub value: u32,
}
#[repr(C)]
#[repr(align(4))]
#[derive(Debug, Copy, Clone)]
pub struct _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 3usize]>,
    pub __bindgen_padding_0: u8,
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1"][::core::mem::size_of::<
        _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1,
    >() - 4usize];
    ["Alignment of _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1"][::core::mem::align_of::<
        _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1,
    >() - 4usize];
};
impl _VAPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
    #[inline]
    pub fn chroma_format_idc(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(0usize, 2u8) as u32) }
    }
    #[inline]
    pub fn set_chroma_format_idc(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(0usize, 2u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn chroma_format_idc_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                0usize,
                2u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_chroma_format_idc_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                0usize,
                2u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn residual_colour_transform_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(2usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_residual_colour_transform_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(2usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn residual_colour_transform_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                2usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_residual_colour_transform_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                2usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn gaps_in_frame_num_value_allowed_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(3usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_gaps_in_frame_num_value_allowed_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(3usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn gaps_in_frame_num_value_allowed_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                3usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_gaps_in_frame_num_value_allowed_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                3usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn frame_mbs_only_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(4usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_frame_mbs_only_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(4usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn frame_mbs_only_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                4usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_frame_mbs_only_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                4usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn mb_adaptive_frame_field_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(5usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_mb_adaptive_frame_field_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(5usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn mb_adaptive_frame_field_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                5usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_mb_adaptive_frame_field_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                5usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn direct_8x8_inference_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(6usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_direct_8x8_inference_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(6usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn direct_8x8_inference_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                6usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_direct_8x8_inference_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                6usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn MinLumaBiPredSize8x8(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(7usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_MinLumaBiPredSize8x8(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(7usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn MinLumaBiPredSize8x8_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                7usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_MinLumaBiPredSize8x8_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                7usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn log2_max_frame_num_minus4(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(8usize, 4u8) as u32) }
    }
    #[inline]
    pub fn set_log2_max_frame_num_minus4(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(8usize, 4u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn log2_max_frame_num_minus4_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                8usize,
                4u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_log2_max_frame_num_minus4_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                8usize,
                4u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn pic_order_cnt_type(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(12usize, 2u8) as u32) }
    }
    #[inline]
    pub fn set_pic_order_cnt_type(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(12usize, 2u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn pic_order_cnt_type_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                12usize,
                2u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_pic_order_cnt_type_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                12usize,
                2u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn log2_max_pic_order_cnt_lsb_minus4(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(14usize, 4u8) as u32) }
    }
    #[inline]
    pub fn set_log2_max_pic_order_cnt_lsb_minus4(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(14usize, 4u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn log2_max_pic_order_cnt_lsb_minus4_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                14usize,
                4u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_log2_max_pic_order_cnt_lsb_minus4_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                14usize,
                4u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn delta_pic_order_always_zero_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(18usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_delta_pic_order_always_zero_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(18usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn delta_pic_order_always_zero_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 3usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                18usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_delta_pic_order_always_zero_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 3usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                18usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn new_bitfield_1(
        chroma_format_idc: u32,
        residual_colour_transform_flag: u32,
        gaps_in_frame_num_value_allowed_flag: u32,
        frame_mbs_only_flag: u32,
        mb_adaptive_frame_field_flag: u32,
        direct_8x8_inference_flag: u32,
        MinLumaBiPredSize8x8: u32,
        log2_max_frame_num_minus4: u32,
        pic_order_cnt_type: u32,
        log2_max_pic_order_cnt_lsb_minus4: u32,
        delta_pic_order_always_zero_flag: u32,
    ) -> __BindgenBitfieldUnit<[u8; 3usize]> {
        let mut __bindgen_bitfield_unit: __BindgenBitfieldUnit<[u8; 3usize]> = Default::default();
        __bindgen_bitfield_unit.set(0usize, 2u8, {
            let chroma_format_idc: u32 = unsafe { ::core::mem::transmute(chroma_format_idc) };
            chroma_format_idc as u64
        });
        __bindgen_bitfield_unit.set(2usize, 1u8, {
            let residual_colour_transform_flag: u32 =
                unsafe { ::core::mem::transmute(residual_colour_transform_flag) };
            residual_colour_transform_flag as u64
        });
        __bindgen_bitfield_unit.set(3usize, 1u8, {
            let gaps_in_frame_num_value_allowed_flag: u32 =
                unsafe { ::core::mem::transmute(gaps_in_frame_num_value_allowed_flag) };
            gaps_in_frame_num_value_allowed_flag as u64
        });
        __bindgen_bitfield_unit.set(4usize, 1u8, {
            let frame_mbs_only_flag: u32 = unsafe { ::core::mem::transmute(frame_mbs_only_flag) };
            frame_mbs_only_flag as u64
        });
        __bindgen_bitfield_unit.set(5usize, 1u8, {
            let mb_adaptive_frame_field_flag: u32 =
                unsafe { ::core::mem::transmute(mb_adaptive_frame_field_flag) };
            mb_adaptive_frame_field_flag as u64
        });
        __bindgen_bitfield_unit.set(6usize, 1u8, {
            let direct_8x8_inference_flag: u32 =
                unsafe { ::core::mem::transmute(direct_8x8_inference_flag) };
            direct_8x8_inference_flag as u64
        });
        __bindgen_bitfield_unit.set(7usize, 1u8, {
            let MinLumaBiPredSize8x8: u32 = unsafe { ::core::mem::transmute(MinLumaBiPredSize8x8) };
            MinLumaBiPredSize8x8 as u64
        });
        __bindgen_bitfield_unit.set(8usize, 4u8, {
            let log2_max_frame_num_minus4: u32 =
                unsafe { ::core::mem::transmute(log2_max_frame_num_minus4) };
            log2_max_frame_num_minus4 as u64
        });
        __bindgen_bitfield_unit.set(12usize, 2u8, {
            let pic_order_cnt_type: u32 = unsafe { ::core::mem::transmute(pic_order_cnt_type) };
            pic_order_cnt_type as u64
        });
        __bindgen_bitfield_unit.set(14usize, 4u8, {
            let log2_max_pic_order_cnt_lsb_minus4: u32 =
                unsafe { ::core::mem::transmute(log2_max_pic_order_cnt_lsb_minus4) };
            log2_max_pic_order_cnt_lsb_minus4 as u64
        });
        __bindgen_bitfield_unit.set(18usize, 1u8, {
            let delta_pic_order_always_zero_flag: u32 =
                unsafe { ::core::mem::transmute(delta_pic_order_always_zero_flag) };
            delta_pic_order_always_zero_flag as u64
        });
        __bindgen_bitfield_unit
    }
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAPictureParameterBufferH264__bindgen_ty_1"]
        [::core::mem::size_of::<_VAPictureParameterBufferH264__bindgen_ty_1>() - 4usize];
    ["Alignment of _VAPictureParameterBufferH264__bindgen_ty_1"]
        [::core::mem::align_of::<_VAPictureParameterBufferH264__bindgen_ty_1>() - 4usize];
    ["Offset of field: _VAPictureParameterBufferH264__bindgen_ty_1::bits"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264__bindgen_ty_1, bits) - 0usize];
    ["Offset of field: _VAPictureParameterBufferH264__bindgen_ty_1::value"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264__bindgen_ty_1, value) - 0usize];
};
#[repr(C)]
#[derive(Copy, Clone)]
pub union _VAPictureParameterBufferH264__bindgen_ty_2 {
    pub bits: _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1,
    pub value: u32,
}
#[repr(C)]
#[repr(align(4))]
#[derive(Debug, Copy, Clone)]
pub struct _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1 {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 2usize]>,
    pub __bindgen_padding_0: u16,
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1"][::core::mem::size_of::<
        _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1,
    >() - 4usize];
    ["Alignment of _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1"][::core::mem::align_of::<
        _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1,
    >() - 4usize];
};
impl _VAPictureParameterBufferH264__bindgen_ty_2__bindgen_ty_1 {
    #[inline]
    pub fn entropy_coding_mode_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(0usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_entropy_coding_mode_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(0usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn entropy_coding_mode_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                0usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_entropy_coding_mode_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                0usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn weighted_pred_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(1usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_weighted_pred_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(1usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn weighted_pred_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                1usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_weighted_pred_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                1usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn weighted_bipred_idc(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(2usize, 2u8) as u32) }
    }
    #[inline]
    pub fn set_weighted_bipred_idc(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(2usize, 2u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn weighted_bipred_idc_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                2usize,
                2u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_weighted_bipred_idc_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                2usize,
                2u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn transform_8x8_mode_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(4usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_transform_8x8_mode_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(4usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn transform_8x8_mode_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                4usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_transform_8x8_mode_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                4usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn field_pic_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(5usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_field_pic_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(5usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn field_pic_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                5usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_field_pic_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                5usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn constrained_intra_pred_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(6usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_constrained_intra_pred_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(6usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn constrained_intra_pred_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                6usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_constrained_intra_pred_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                6usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn pic_order_present_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(7usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_pic_order_present_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(7usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn pic_order_present_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                7usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_pic_order_present_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                7usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn deblocking_filter_control_present_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(8usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_deblocking_filter_control_present_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(8usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn deblocking_filter_control_present_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                8usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_deblocking_filter_control_present_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                8usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn redundant_pic_cnt_present_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(9usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_redundant_pic_cnt_present_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(9usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn redundant_pic_cnt_present_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                9usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_redundant_pic_cnt_present_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                9usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn reference_pic_flag(&self) -> u32 {
        unsafe { ::core::mem::transmute(self._bitfield_1.get(10usize, 1u8) as u32) }
    }
    #[inline]
    pub fn set_reference_pic_flag(&mut self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            self._bitfield_1.set(10usize, 1u8, val as u64)
        }
    }
    #[inline]
    pub unsafe fn reference_pic_flag_raw(this: *const Self) -> u32 {
        unsafe {
            ::core::mem::transmute(<__BindgenBitfieldUnit<[u8; 2usize]>>::raw_get(
                ::core::ptr::addr_of!((*this)._bitfield_1),
                10usize,
                1u8,
            ) as u32)
        }
    }
    #[inline]
    pub unsafe fn set_reference_pic_flag_raw(this: *mut Self, val: u32) {
        unsafe {
            let val: u32 = ::core::mem::transmute(val);
            <__BindgenBitfieldUnit<[u8; 2usize]>>::raw_set(
                ::core::ptr::addr_of_mut!((*this)._bitfield_1),
                10usize,
                1u8,
                val as u64,
            )
        }
    }
    #[inline]
    pub fn new_bitfield_1(
        entropy_coding_mode_flag: u32,
        weighted_pred_flag: u32,
        weighted_bipred_idc: u32,
        transform_8x8_mode_flag: u32,
        field_pic_flag: u32,
        constrained_intra_pred_flag: u32,
        pic_order_present_flag: u32,
        deblocking_filter_control_present_flag: u32,
        redundant_pic_cnt_present_flag: u32,
        reference_pic_flag: u32,
    ) -> __BindgenBitfieldUnit<[u8; 2usize]> {
        let mut __bindgen_bitfield_unit: __BindgenBitfieldUnit<[u8; 2usize]> = Default::default();
        __bindgen_bitfield_unit.set(0usize, 1u8, {
            let entropy_coding_mode_flag: u32 =
                unsafe { ::core::mem::transmute(entropy_coding_mode_flag) };
            entropy_coding_mode_flag as u64
        });
        __bindgen_bitfield_unit.set(1usize, 1u8, {
            let weighted_pred_flag: u32 = unsafe { ::core::mem::transmute(weighted_pred_flag) };
            weighted_pred_flag as u64
        });
        __bindgen_bitfield_unit.set(2usize, 2u8, {
            let weighted_bipred_idc: u32 = unsafe { ::core::mem::transmute(weighted_bipred_idc) };
            weighted_bipred_idc as u64
        });
        __bindgen_bitfield_unit.set(4usize, 1u8, {
            let transform_8x8_mode_flag: u32 =
                unsafe { ::core::mem::transmute(transform_8x8_mode_flag) };
            transform_8x8_mode_flag as u64
        });
        __bindgen_bitfield_unit.set(5usize, 1u8, {
            let field_pic_flag: u32 = unsafe { ::core::mem::transmute(field_pic_flag) };
            field_pic_flag as u64
        });
        __bindgen_bitfield_unit.set(6usize, 1u8, {
            let constrained_intra_pred_flag: u32 =
                unsafe { ::core::mem::transmute(constrained_intra_pred_flag) };
            constrained_intra_pred_flag as u64
        });
        __bindgen_bitfield_unit.set(7usize, 1u8, {
            let pic_order_present_flag: u32 =
                unsafe { ::core::mem::transmute(pic_order_present_flag) };
            pic_order_present_flag as u64
        });
        __bindgen_bitfield_unit.set(8usize, 1u8, {
            let deblocking_filter_control_present_flag: u32 =
                unsafe { ::core::mem::transmute(deblocking_filter_control_present_flag) };
            deblocking_filter_control_present_flag as u64
        });
        __bindgen_bitfield_unit.set(9usize, 1u8, {
            let redundant_pic_cnt_present_flag: u32 =
                unsafe { ::core::mem::transmute(redundant_pic_cnt_present_flag) };
            redundant_pic_cnt_present_flag as u64
        });
        __bindgen_bitfield_unit.set(10usize, 1u8, {
            let reference_pic_flag: u32 = unsafe { ::core::mem::transmute(reference_pic_flag) };
            reference_pic_flag as u64
        });
        __bindgen_bitfield_unit
    }
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAPictureParameterBufferH264__bindgen_ty_2"]
        [::core::mem::size_of::<_VAPictureParameterBufferH264__bindgen_ty_2>() - 4usize];
    ["Alignment of _VAPictureParameterBufferH264__bindgen_ty_2"]
        [::core::mem::align_of::<_VAPictureParameterBufferH264__bindgen_ty_2>() - 4usize];
    ["Offset of field: _VAPictureParameterBufferH264__bindgen_ty_2::bits"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264__bindgen_ty_2, bits) - 0usize];
    ["Offset of field: _VAPictureParameterBufferH264__bindgen_ty_2::value"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264__bindgen_ty_2, value) - 0usize];
};
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAPictureParameterBufferH264"]
        [::core::mem::size_of::<_VAPictureParameterBufferH264>() - 672usize];
    ["Alignment of _VAPictureParameterBufferH264"]
        [::core::mem::align_of::<_VAPictureParameterBufferH264>() - 4usize];
    ["Offset of field: _VAPictureParameterBufferH264::CurrPic"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, CurrPic) - 0usize];
    ["Offset of field: _VAPictureParameterBufferH264::ReferenceFrames"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, ReferenceFrames) - 36usize];
    ["Offset of field: _VAPictureParameterBufferH264::picture_width_in_mbs_minus1"][::core::mem::offset_of!(
        _VAPictureParameterBufferH264,
        picture_width_in_mbs_minus1
    ) - 612usize];
    ["Offset of field: _VAPictureParameterBufferH264::picture_height_in_mbs_minus1"][::core::mem::offset_of!(
        _VAPictureParameterBufferH264,
        picture_height_in_mbs_minus1
    ) - 614usize];
    ["Offset of field: _VAPictureParameterBufferH264::bit_depth_luma_minus8"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, bit_depth_luma_minus8) - 616usize];
    ["Offset of field: _VAPictureParameterBufferH264::bit_depth_chroma_minus8"][::core::mem::offset_of!(
        _VAPictureParameterBufferH264,
        bit_depth_chroma_minus8
    ) - 617usize];
    ["Offset of field: _VAPictureParameterBufferH264::num_ref_frames"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, num_ref_frames) - 618usize];
    ["Offset of field: _VAPictureParameterBufferH264::seq_fields"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, seq_fields) - 620usize];
    ["Offset of field: _VAPictureParameterBufferH264::num_slice_groups_minus1"][::core::mem::offset_of!(
        _VAPictureParameterBufferH264,
        num_slice_groups_minus1
    ) - 624usize];
    ["Offset of field: _VAPictureParameterBufferH264::slice_group_map_type"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, slice_group_map_type) - 625usize];
    ["Offset of field: _VAPictureParameterBufferH264::slice_group_change_rate_minus1"][::core::mem::offset_of!(
        _VAPictureParameterBufferH264,
        slice_group_change_rate_minus1
    )
        - 626usize];
    ["Offset of field: _VAPictureParameterBufferH264::pic_init_qp_minus26"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, pic_init_qp_minus26) - 628usize];
    ["Offset of field: _VAPictureParameterBufferH264::pic_init_qs_minus26"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, pic_init_qs_minus26) - 629usize];
    ["Offset of field: _VAPictureParameterBufferH264::chroma_qp_index_offset"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, chroma_qp_index_offset) - 630usize];
    ["Offset of field: _VAPictureParameterBufferH264::second_chroma_qp_index_offset"][::core::mem::offset_of!(
        _VAPictureParameterBufferH264,
        second_chroma_qp_index_offset
    ) - 631usize];
    ["Offset of field: _VAPictureParameterBufferH264::pic_fields"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, pic_fields) - 632usize];
    ["Offset of field: _VAPictureParameterBufferH264::frame_num"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, frame_num) - 636usize];
    ["Offset of field: _VAPictureParameterBufferH264::va_reserved"]
        [::core::mem::offset_of!(_VAPictureParameterBufferH264, va_reserved) - 640usize];
};
pub type VAPictureParameterBufferH264 = _VAPictureParameterBufferH264;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _VAIQMatrixBufferH264 {
    pub ScalingList4x4: [[u8; 16usize]; 6usize],
    pub ScalingList8x8: [[u8; 64usize]; 2usize],
    pub va_reserved: [u32; 4usize],
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAIQMatrixBufferH264"][::core::mem::size_of::<_VAIQMatrixBufferH264>() - 240usize];
    ["Alignment of _VAIQMatrixBufferH264"]
        [::core::mem::align_of::<_VAIQMatrixBufferH264>() - 4usize];
    ["Offset of field: _VAIQMatrixBufferH264::ScalingList4x4"]
        [::core::mem::offset_of!(_VAIQMatrixBufferH264, ScalingList4x4) - 0usize];
    ["Offset of field: _VAIQMatrixBufferH264::ScalingList8x8"]
        [::core::mem::offset_of!(_VAIQMatrixBufferH264, ScalingList8x8) - 96usize];
    ["Offset of field: _VAIQMatrixBufferH264::va_reserved"]
        [::core::mem::offset_of!(_VAIQMatrixBufferH264, va_reserved) - 224usize];
};
pub type VAIQMatrixBufferH264 = _VAIQMatrixBufferH264;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _VASliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub RefPicList0: [VAPictureH264; 32usize],
    pub RefPicList1: [VAPictureH264; 32usize],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32usize],
    pub luma_offset_l0: [i16; 32usize],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2usize]; 32usize],
    pub chroma_offset_l0: [[i16; 2usize]; 32usize],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32usize],
    pub luma_offset_l1: [i16; 32usize],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2usize]; 32usize],
    pub chroma_offset_l1: [[i16; 2usize]; 32usize],
    pub va_reserved: [u32; 4usize],
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VASliceParameterBufferH264"]
        [::core::mem::size_of::<_VASliceParameterBufferH264>() - 3128usize];
    ["Alignment of _VASliceParameterBufferH264"]
        [::core::mem::align_of::<_VASliceParameterBufferH264>() - 4usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_data_size"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_data_size) - 0usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_data_offset"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_data_offset) - 4usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_data_flag"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_data_flag) - 8usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_data_bit_offset"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_data_bit_offset) - 12usize];
    ["Offset of field: _VASliceParameterBufferH264::first_mb_in_slice"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, first_mb_in_slice) - 14usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_type"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_type) - 16usize];
    ["Offset of field: _VASliceParameterBufferH264::direct_spatial_mv_pred_flag"][::core::mem::offset_of!(
        _VASliceParameterBufferH264,
        direct_spatial_mv_pred_flag
    ) - 17usize];
    ["Offset of field: _VASliceParameterBufferH264::num_ref_idx_l0_active_minus1"][::core::mem::offset_of!(
        _VASliceParameterBufferH264,
        num_ref_idx_l0_active_minus1
    ) - 18usize];
    ["Offset of field: _VASliceParameterBufferH264::num_ref_idx_l1_active_minus1"][::core::mem::offset_of!(
        _VASliceParameterBufferH264,
        num_ref_idx_l1_active_minus1
    ) - 19usize];
    ["Offset of field: _VASliceParameterBufferH264::cabac_init_idc"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, cabac_init_idc) - 20usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_qp_delta"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_qp_delta) - 21usize];
    ["Offset of field: _VASliceParameterBufferH264::disable_deblocking_filter_idc"][::core::mem::offset_of!(
        _VASliceParameterBufferH264,
        disable_deblocking_filter_idc
    ) - 22usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_alpha_c0_offset_div2"][::core::mem::offset_of!(
        _VASliceParameterBufferH264,
        slice_alpha_c0_offset_div2
    ) - 23usize];
    ["Offset of field: _VASliceParameterBufferH264::slice_beta_offset_div2"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, slice_beta_offset_div2) - 24usize];
    ["Offset of field: _VASliceParameterBufferH264::RefPicList0"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, RefPicList0) - 28usize];
    ["Offset of field: _VASliceParameterBufferH264::RefPicList1"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, RefPicList1) - 1180usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_log2_weight_denom"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_log2_weight_denom) - 2332usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_log2_weight_denom"][::core::mem::offset_of!(
        _VASliceParameterBufferH264,
        chroma_log2_weight_denom
    ) - 2333usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_weight_l0_flag"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_weight_l0_flag) - 2334usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_weight_l0"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_weight_l0) - 2336usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_offset_l0"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_offset_l0) - 2400usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_weight_l0_flag"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, chroma_weight_l0_flag) - 2464usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_weight_l0"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, chroma_weight_l0) - 2466usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_offset_l0"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, chroma_offset_l0) - 2594usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_weight_l1_flag"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_weight_l1_flag) - 2722usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_weight_l1"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_weight_l1) - 2724usize];
    ["Offset of field: _VASliceParameterBufferH264::luma_offset_l1"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, luma_offset_l1) - 2788usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_weight_l1_flag"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, chroma_weight_l1_flag) - 2852usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_weight_l1"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, chroma_weight_l1) - 2854usize];
    ["Offset of field: _VASliceParameterBufferH264::chroma_offset_l1"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, chroma_offset_l1) - 2982usize];
    ["Offset of field: _VASliceParameterBufferH264::va_reserved"]
        [::core::mem::offset_of!(_VASliceParameterBufferH264, va_reserved) - 3112usize];
};
pub type VASliceParameterBufferH264 = _VASliceParameterBufferH264;
pub const VASurfaceRendering: VASurfaceStatus = 1;
pub const VASurfaceDisplaying: VASurfaceStatus = 2;
pub const VASurfaceReady: VASurfaceStatus = 4;
pub const VASurfaceSkipped: VASurfaceStatus = 8;
pub type VASurfaceStatus = ::core::ffi::c_uint;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4usize],
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAImageFormat"][::core::mem::size_of::<_VAImageFormat>() - 48usize];
    ["Alignment of _VAImageFormat"][::core::mem::align_of::<_VAImageFormat>() - 4usize];
    ["Offset of field: _VAImageFormat::fourcc"]
        [::core::mem::offset_of!(_VAImageFormat, fourcc) - 0usize];
    ["Offset of field: _VAImageFormat::byte_order"]
        [::core::mem::offset_of!(_VAImageFormat, byte_order) - 4usize];
    ["Offset of field: _VAImageFormat::bits_per_pixel"]
        [::core::mem::offset_of!(_VAImageFormat, bits_per_pixel) - 8usize];
    ["Offset of field: _VAImageFormat::depth"]
        [::core::mem::offset_of!(_VAImageFormat, depth) - 12usize];
    ["Offset of field: _VAImageFormat::red_mask"]
        [::core::mem::offset_of!(_VAImageFormat, red_mask) - 16usize];
    ["Offset of field: _VAImageFormat::green_mask"]
        [::core::mem::offset_of!(_VAImageFormat, green_mask) - 20usize];
    ["Offset of field: _VAImageFormat::blue_mask"]
        [::core::mem::offset_of!(_VAImageFormat, blue_mask) - 24usize];
    ["Offset of field: _VAImageFormat::alpha_mask"]
        [::core::mem::offset_of!(_VAImageFormat, alpha_mask) - 28usize];
    ["Offset of field: _VAImageFormat::va_reserved"]
        [::core::mem::offset_of!(_VAImageFormat, va_reserved) - 32usize];
};
pub type VAImageFormat = _VAImageFormat;
pub type VAImageID = VAGenericID;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct _VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3usize],
    pub offsets: [u32; 3usize],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4usize],
    pub va_reserved: [u32; 4usize],
}
#[allow(clippy::unnecessary_operation, clippy::identity_op)]
const _: () = {
    ["Size of _VAImage"][::core::mem::size_of::<_VAImage>() - 120usize];
    ["Alignment of _VAImage"][::core::mem::align_of::<_VAImage>() - 4usize];
    ["Offset of field: _VAImage::image_id"][::core::mem::offset_of!(_VAImage, image_id) - 0usize];
    ["Offset of field: _VAImage::format"][::core::mem::offset_of!(_VAImage, format) - 4usize];
    ["Offset of field: _VAImage::buf"][::core::mem::offset_of!(_VAImage, buf) - 52usize];
    ["Offset of field: _VAImage::width"][::core::mem::offset_of!(_VAImage, width) - 56usize];
    ["Offset of field: _VAImage::height"][::core::mem::offset_of!(_VAImage, height) - 58usize];
    ["Offset of field: _VAImage::data_size"]
        [::core::mem::offset_of!(_VAImage, data_size) - 60usize];
    ["Offset of field: _VAImage::num_planes"]
        [::core::mem::offset_of!(_VAImage, num_planes) - 64usize];
    ["Offset of field: _VAImage::pitches"][::core::mem::offset_of!(_VAImage, pitches) - 68usize];
    ["Offset of field: _VAImage::offsets"][::core::mem::offset_of!(_VAImage, offsets) - 80usize];
    ["Offset of field: _VAImage::num_palette_entries"]
        [::core::mem::offset_of!(_VAImage, num_palette_entries) - 92usize];
    ["Offset of field: _VAImage::entry_bytes"]
        [::core::mem::offset_of!(_VAImage, entry_bytes) - 96usize];
    ["Offset of field: _VAImage::component_order"]
        [::core::mem::offset_of!(_VAImage, component_order) - 100usize];
    ["Offset of field: _VAImage::va_reserved"]
        [::core::mem::offset_of!(_VAImage, va_reserved) - 104usize];
};
pub type VAImage = _VAImage;

use core::ffi::{c_char, c_int, c_uint, c_void};

/// libva's entry points, from the libraries found at runtime.
pub struct Va {
    _va: libloading::Library,
    _drm: libloading::Library,
    pub vaGetDisplayDRM: unsafe extern "C" fn(fd: c_int) -> VADisplay,
    pub vaInitialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    pub vaTerminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    pub vaQueryVendorString: unsafe extern "C" fn(VADisplay) -> *const c_char,
    pub vaErrorStr: unsafe extern "C" fn(VAStatus) -> *const c_char,
    pub vaSetInfoCallback: unsafe extern "C" fn(VADisplay, Option<unsafe extern "C" fn(*mut c_void, *const c_char)>, *mut c_void) -> *mut c_void,
    pub vaSetErrorCallback: unsafe extern "C" fn(VADisplay, Option<unsafe extern "C" fn(*mut c_void, *const c_char)>, *mut c_void) -> *mut c_void,
    pub vaMaxNumProfiles: unsafe extern "C" fn(VADisplay) -> c_int,
    pub vaQueryConfigProfiles: unsafe extern "C" fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus,
    pub vaMaxNumEntrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    pub vaQueryConfigEntrypoints: unsafe extern "C" fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus,
    pub vaCreateConfig: unsafe extern "C" fn(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus,
    pub vaDestroyConfig: unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus,
    pub vaCreateSurfaces: unsafe extern "C" fn(VADisplay, c_uint, c_uint, c_uint, *mut VASurfaceID, c_uint, *mut VASurfaceAttrib, c_uint) -> VAStatus,
    pub vaDestroySurfaces: unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus,
    pub vaCreateContext: unsafe extern "C" fn(VADisplay, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus,
    pub vaDestroyContext: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    pub vaCreateBuffer: unsafe extern "C" fn(VADisplay, VAContextID, VABufferType, c_uint, c_uint, *mut c_void, *mut VABufferID) -> VAStatus,
    pub vaDestroyBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    pub vaBeginPicture: unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus,
    pub vaRenderPicture: unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus,
    pub vaEndPicture: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    pub vaSyncSurface: unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus,
    pub vaDeriveImage: unsafe extern "C" fn(VADisplay, VASurfaceID, *mut VAImage) -> VAStatus,
    pub vaCreateImage: unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus,
    pub vaGetImage: unsafe extern "C" fn(VADisplay, VASurfaceID, c_int, c_int, c_uint, c_uint, VAImageID) -> VAStatus,
    pub vaDestroyImage: unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus,
    pub vaMapBuffer: unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus,
    pub vaUnmapBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
}

impl Va {
    /// Loads libva, or says why it can't.
    ///
    /// # Safety
    /// The libraries are libva's: each symbol has the type its header gives it.
    unsafe fn load() -> Result<Va, String> {
        unsafe {
            let va = libloading::Library::new("libva.so.2").map_err(|e| e.to_string())?;
            let drm = libloading::Library::new("libva-drm.so.2").map_err(|e| e.to_string())?;
            macro_rules! sym {
                ($lib:expr, $name:literal) => {
                    *$lib.get(concat!($name, "\0").as_bytes()).map_err(|e| format!("{}: {e}", $name))?
                };
            }
            Ok(Va {
                vaGetDisplayDRM: sym!(drm, "vaGetDisplayDRM"),
                vaInitialize: sym!(va, "vaInitialize"),
                vaTerminate: sym!(va, "vaTerminate"),
                vaQueryVendorString: sym!(va, "vaQueryVendorString"),
                vaErrorStr: sym!(va, "vaErrorStr"),
                vaSetInfoCallback: sym!(va, "vaSetInfoCallback"),
                vaSetErrorCallback: sym!(va, "vaSetErrorCallback"),
                vaMaxNumProfiles: sym!(va, "vaMaxNumProfiles"),
                vaQueryConfigProfiles: sym!(va, "vaQueryConfigProfiles"),
                vaMaxNumEntrypoints: sym!(va, "vaMaxNumEntrypoints"),
                vaQueryConfigEntrypoints: sym!(va, "vaQueryConfigEntrypoints"),
                vaCreateConfig: sym!(va, "vaCreateConfig"),
                vaDestroyConfig: sym!(va, "vaDestroyConfig"),
                vaCreateSurfaces: sym!(va, "vaCreateSurfaces"),
                vaDestroySurfaces: sym!(va, "vaDestroySurfaces"),
                vaCreateContext: sym!(va, "vaCreateContext"),
                vaDestroyContext: sym!(va, "vaDestroyContext"),
                vaCreateBuffer: sym!(va, "vaCreateBuffer"),
                vaDestroyBuffer: sym!(va, "vaDestroyBuffer"),
                vaBeginPicture: sym!(va, "vaBeginPicture"),
                vaRenderPicture: sym!(va, "vaRenderPicture"),
                vaEndPicture: sym!(va, "vaEndPicture"),
                vaSyncSurface: sym!(va, "vaSyncSurface"),
                vaDeriveImage: sym!(va, "vaDeriveImage"),
                vaCreateImage: sym!(va, "vaCreateImage"),
                vaGetImage: sym!(va, "vaGetImage"),
                vaDestroyImage: sym!(va, "vaDestroyImage"),
                vaMapBuffer: sym!(va, "vaMapBuffer"),
                vaUnmapBuffer: sym!(va, "vaUnmapBuffer"),
                _va: va,
                _drm: drm,
            })
        }
    }

    /// libva, loaded once; why not, if it isn't there.
    pub fn get() -> Result<&'static Va, &'static str> {
        static VA: std::sync::OnceLock<Result<Va, String>> = std::sync::OnceLock::new();
        // SAFETY: libva's own libraries, by their soname.
        VA.get_or_init(|| unsafe { Va::load() }).as_ref().map_err(String::as_str)
    }

    /// libva's words for a status.
    pub fn error(&self, status: VAStatus) -> String {
        // SAFETY: libva returns a static string for any status.
        let text = unsafe { (self.vaErrorStr)(status) };
        if text.is_null() { format!("VA status {status}") } else { unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned() }
    }
}
