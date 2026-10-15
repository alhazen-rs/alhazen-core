//! The CUDA kernel that converts NVDEC's pictures to BGRA on the GPU: colour matrix and range,
//! scaling to the display size, and HDR → SDR tone mapping (`kernels/convert.cu`, shipped as PTX
//! and compiled by the driver when first loaded).

/// FNV-1a 64 of the kernel source: `scripts/build-ptx.sh` records it in the PTX, so a PTX not
/// rebuilt after an edit is caught.
#[cfg(test)]
fn source_hash(source: &str) -> u64 {
    source.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

#[cfg(all(target_os = "linux", feature = "nvdec"))]
pub(crate) use gpu::{Converter, Source};

#[cfg(all(target_os = "linux", feature = "nvdec"))]
mod gpu {
    use std::ffi::c_void;

    use super::super::device::check;
    use super::super::sys::*;
    use crate::color::tonemap::{SDR_WHITE_NITS, pq_inverse_eotf, source_peak};
    use crate::color::{ColorInfo, ColorMatrix, Primaries, Transfer};
    use crate::{Error, Result};

    /// The kernel's parameter block: the same fields, in the same order, as `Params` in
    /// `convert.cu`.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Params {
        src_w: i32,
        src_h: i32,
        pitch: i32,
        chroma_offset: i32,
        ten_bit: i32,
        dst_w: i32,
        dst_h: i32,
        kr_v: f32,
        kg_u: f32,
        kg_v: f32,
        kb_u: f32,
        y_off: f32,
        y_scale: f32,
        c_off: f32,
        c_scale: f32,
        transfer: i32,
        bt2020: i32,
        src_peak_pq: f32,
        sdr_white_pq: f32,
        peak: f32,
    }

    /// One mapped NVDEC picture: NV12 (8-bit) or P016 (10-bit, MSB-aligned) at `ptr`.
    pub(crate) struct Source {
        pub ptr: CUdeviceptr,
        pub pitch: usize,
        pub size: (u32, u32),
        /// Bytes from `ptr` to the interleaved chroma plane.
        pub chroma_offset: usize,
        pub ten_bit: bool,
    }

    /// The loaded kernel and its buffers. Used from inside NVDEC's callbacks, with the decoder's
    /// CUDA context current.
    pub(crate) struct Converter {
        api: &'static Api,
        module: CUmodule,
        function: CUfunction,
        device: (CUdeviceptr, usize),
        host: (*mut u8, usize),
        pool: crate::frame::FramePool,
    }

    impl Converter {
        pub fn load(api: &'static Api) -> Result<Converter> {
            if cfg!(debug_assertions) && std::env::var_os("ALHAZEN_TEST_BREAK_PTX").is_some() {
                return Err(Error::Decode("NVDEC kernel: broken on purpose (ALHAZEN_TEST_BREAK_PTX)".into()));
            }
            let ptx = concat!(include_str!("kernels/convert.ptx"), "\0");
            let mut module = std::ptr::null_mut();
            // SAFETY: a NUL-terminated PTX image; the out-pointer receives a module we own.
            check(unsafe { (api.cuModuleLoadData)(&mut module, ptx.as_ptr().cast()) }, "load the conversion kernel")?;
            let mut function = std::ptr::null_mut();
            // SAFETY: a live module and a NUL-terminated name.
            let found = check(unsafe { (api.cuModuleGetFunction)(&mut function, module, c"convert".as_ptr()) }, "find the conversion kernel");
            if let Err(e) = found {
                // SAFETY: the module loaded above, unloaded once.
                unsafe { (api.cuModuleUnload)(module) };
                return Err(e);
            }
            Ok(Converter { api, module, function, device: (0, 0), host: (std::ptr::null_mut(), 0), pool: Default::default() })
        }

        /// `src` converted to BGRA at `dst` (width, height).
        pub fn convert(&mut self, src: &Source, color: &ColorInfo, dst: (u32, u32)) -> Result<std::sync::Arc<[u8]>> {
            let api = self.api;
            let len = dst.0 as usize * dst.1 as usize * 4;
            if self.device.1 < len {
                self.free_device();
                let mut ptr = 0;
                // SAFETY: out-pointer valid; context current.
                check(unsafe { (api.cuMemAlloc)(&mut ptr, len) }, "allocate the BGRA buffer")?;
                self.device = (ptr, len);
            }
            if self.host.1 < len {
                self.free_host();
                let mut ptr = std::ptr::null_mut();
                // SAFETY: out-pointer valid; context current.
                check(unsafe { (api.cuMemAllocHost)(&mut ptr, len) }, "allocate the host buffer")?;
                self.host = (ptr.cast(), len);
            }
            let mut params = params(src, color, dst);
            let (mut input, mut output) = (src.ptr, self.device.0);
            let mut args: [*mut c_void; 3] =
                [(&raw mut input).cast(), (&raw mut output).cast(), (&raw mut params).cast()];
            let (bx, by) = (16, 16);
            let grid = (dst.0.div_ceil(bx), dst.1.div_ceil(by));
            // SAFETY: the kernel's three parameters (two device pointers and `Params`), each
            // pointed to by `args`; launched on the default stream with the context current.
            check(
                unsafe {
                    (api.cuLaunchKernel)(self.function, grid.0, grid.1, 1, bx, by, 1, 0, std::ptr::null_mut(), args.as_mut_ptr(), std::ptr::null_mut())
                },
                "launch the conversion kernel",
            )?;
            let mut m: CUDA_MEMCPY2D = zeroed();
            m.srcMemoryType = CU_MEMORYTYPE_DEVICE;
            m.srcDevice = self.device.0;
            m.srcPitch = dst.0 as usize * 4;
            m.dstMemoryType = CU_MEMORYTYPE_HOST;
            m.dstHost = self.host.0.cast();
            m.dstPitch = dst.0 as usize * 4;
            m.WidthInBytes = dst.0 as usize * 4;
            m.Height = dst.1 as usize;
            // SAFETY: both buffers hold `len` bytes; the copy waits for the kernel (same stream).
            check(unsafe { (api.cuMemcpy2D)(&m) }, "copy the BGRA picture")?;
            // SAFETY: the pinned buffer holds the `len` bytes just copied.
            let picture = unsafe { std::slice::from_raw_parts(self.host.0, len) };
            Ok(self.pool.fill(len, |dst| dst.copy_from_slice(picture)))
        }

        fn free_device(&mut self) {
            if self.device.0 != 0 {
                // SAFETY: allocated by `cuMemAlloc`, freed once.
                unsafe { (self.api.cuMemFree)(self.device.0) };
                self.device = (0, 0);
            }
        }

        fn free_host(&mut self) {
            if !self.host.0.is_null() {
                // SAFETY: allocated by `cuMemAllocHost`, freed once.
                unsafe { (self.api.cuMemFreeHost)(self.host.0.cast()) };
                self.host = (std::ptr::null_mut(), 0);
            }
        }
    }

    impl Drop for Converter {
        fn drop(&mut self) {
            // Dropped with the decoder, whose context is current then (see `NvdecVideoDecoder`).
            self.free_device();
            self.free_host();
            // SAFETY: the module loaded in `load`, unloaded once.
            unsafe { (self.api.cuModuleUnload)(self.module) };
        }
    }

    /// The kernel's parameters for a picture and its colour description.
    fn params(src: &Source, color: &ColorInfo, dst: (u32, u32)) -> Params {
        let [kr_v, kg_u, kg_v, kb_u] = match color.matrix {
            ColorMatrix::Bt601 => [1.402, 0.344_136, 0.714_136, 1.772],
            ColorMatrix::Bt709 => [1.5748, 0.187_324, 0.468_124, 1.8556],
            ColorMatrix::Bt2020 => [1.4746, 0.164_553, 0.571_353, 1.8814],
        };
        // Samples are normalized to 0–1 by the full 8- or 16-bit scale; P016 holds 10-bit values
        // in the top bits, so the code values are scaled by 2^6 there.
        let (unit, depth_scale) = if src.ten_bit { (65535.0f32, 4.0 * 64.0) } else { (255.0f32, 1.0) };
        let (y_off, y_scale, c_off, c_scale) = if color.full_range {
            let max = if src.ten_bit { 1023.0 * 64.0 } else { 255.0 };
            (0.0, unit / max, if src.ten_bit { 512.0 * 64.0 / unit } else { 128.0 / unit }, unit / max)
        } else {
            (16.0 * depth_scale / unit, unit / (219.0 * depth_scale), 128.0 * depth_scale / unit, unit / (224.0 * depth_scale))
        };
        let transfer = match color.transfer {
            Transfer::Sdr => 0,
            Transfer::Pq => 1,
            Transfer::Hlg => 2,
        };
        let peak = source_peak(color.hdr.as_ref());
        Params {
            src_w: src.size.0 as i32,
            src_h: src.size.1 as i32,
            pitch: src.pitch as i32,
            chroma_offset: src.chroma_offset as i32,
            ten_bit: src.ten_bit as i32,
            dst_w: dst.0 as i32,
            dst_h: dst.1 as i32,
            kr_v,
            kg_u,
            kg_v,
            kb_u,
            y_off,
            y_scale,
            c_off,
            c_scale,
            transfer,
            bt2020: (color.primaries == Primaries::Bt2020 && transfer != 0) as i32,
            src_peak_pq: pq_inverse_eotf(peak.max(SDR_WHITE_NITS)) as f32,
            sdr_white_pq: pq_inverse_eotf(SDR_WHITE_NITS) as f32,
            peak: peak as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_ptx_matches_the_cuda_source() {
        let cu = include_str!("kernels/convert.cu");
        let ptx = include_str!("kernels/convert.ptx");
        let header = ptx.lines().next().unwrap();
        assert_eq!(
            header,
            format!("// alhazen-source-fnv1a64: {:016x}", source_hash(cu)),
            "convert.cu changed without regenerating convert.ptx: run scripts/build-ptx.sh"
        );
        assert!(ptx.contains(".target sm_52"), "the PTX must target compute_52 (Maxwell and newer)");
    }
}
