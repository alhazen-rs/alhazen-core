//! The D3D11 video processor: converts Media Foundation's GPU-decoded pictures (NV12/P010
//! textures) to BGRA, scaled to the display size, with the driver's HDR → SDR tone mapping, then
//! copies the result to memory.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1, ID3D11VideoContext2, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_GHLG_TOPLEFT_P2020, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{DXGI_HDR_METADATA_HDR10, DXGI_HDR_METADATA_TYPE_HDR10, DXGI_HDR_METADATA_TYPE_NONE};
use windows::Win32::Media::MediaFoundation::{IMFDXGIBuffer, IMFSample};
use windows::core::Interface;

use crate::color::{ColorInfo, ColorMatrix, Transfer};
use crate::{Error, Result};

fn err(what: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Decode(format!("video processor: {what}: {e}"))
}

/// A video processor for one input size, output size and bit depth (recreated when they change).
pub(crate) struct Processor {
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    staging: ID3D11Texture2D,
    src: (u32, u32),
    dst: (u32, u32),
    /// Test switch (debug builds): this blit fails (`ALHAZEN_TEST_BREAK_VPBLT`).
    break_at: Option<u32>,
    blits: u32,
    pool: crate::frame::FramePool,
}

impl Processor {
    /// For pictures of `src` (visible size) converted to `dst`.
    pub fn new(device: &ID3D11Device, src: (u32, u32), dst: (u32, u32)) -> Result<Processor> {
        // SAFETY: COM calls on live objects; descriptions outlive the calls.
        unsafe {
            let context = device.GetImmediateContext().map_err(err("device context"))?;
            let video_device: ID3D11VideoDevice = device.cast().map_err(err("video device"))?;
            let video_context: ID3D11VideoContext1 =
                context.cast::<ID3D11VideoContext>().and_then(|c| c.cast()).map_err(err("video context"))?;
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: src.0,
                InputHeight: src.1,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: dst.0,
                OutputHeight: dst.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc).map_err(err("enumerator"))?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0).map_err(err("create"))?;
            let texture = |usage, bind: u32, cpu: u32| -> Result<ID3D11Texture2D> {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: dst.0,
                    Height: dst.1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: usage,
                    BindFlags: bind,
                    CPUAccessFlags: cpu,
                    MiscFlags: 0,
                };
                let mut t = None;
                device.CreateTexture2D(&desc, None, Some(&mut t)).map_err(err("texture"))?;
                t.ok_or_else(|| Error::Decode("video processor: no texture".into()))
            };
            let output = texture(D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0)?;
            let staging = texture(D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32)?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output_view = None;
            video_device
                .CreateVideoProcessorOutputView(&output, &enumerator, &view_desc, Some(&mut output_view))
                .map_err(err("output view"))?;
            let output_view = output_view.ok_or_else(|| Error::Decode("video processor: no output view".into()))?;
            let break_at = cfg!(debug_assertions)
                .then(|| std::env::var("ALHAZEN_TEST_BREAK_VPBLT").ok()?.parse().ok())
                .flatten();
            Ok(Processor {
                context,
                video_device,
                video_context,
                enumerator,
                processor,
                output,
                output_view,
                staging,
                src,
                dst,
                break_at,
                blits: 0,
                pool: Default::default(),
            })
        }
    }

    pub fn fits(&self, src: (u32, u32), dst: (u32, u32)) -> bool {
        self.src == src && self.dst == dst
    }

    /// `sample`'s picture (a D3D11 texture) converted to BGRA.
    pub fn convert(&mut self, sample: &IMFSample, color: &ColorInfo) -> Result<std::sync::Arc<[u8]>> {
        self.blits += 1;
        if self.break_at == Some(self.blits) {
            return Err(Error::Decode("video processor: broken on purpose (ALHAZEN_TEST_BREAK_VPBLT)".into()));
        }
        // SAFETY: COM calls on live objects; every pointer passed outlives its call; the
        // staging texture is mapped only between Map and Unmap.
        unsafe {
            let buffer = sample.GetBufferByIndex(0).map_err(err("sample buffer"))?;
            let dxgi: IMFDXGIBuffer = buffer.cast().map_err(err("not a GPU picture"))?;
            let mut texture: Option<ID3D11Texture2D> = None;
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut _).map_err(err("texture"))?;
            let texture = texture.ok_or_else(|| Error::Decode("video processor: no input texture".into()))?;
            let slice = dxgi.GetSubresourceIndex().map_err(err("subresource"))?;
            let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice } },
            };
            let mut input: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(&texture, &self.enumerator, &view_desc, Some(&mut input))
                .map_err(err("input view"))?;
            let vc = &self.video_context;
            let p = &self.processor;
            vc.VideoProcessorSetStreamFrameFormat(p, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vc.VideoProcessorSetStreamColorSpace1(p, 0, input_color_space(color));
            vc.VideoProcessorSetOutputColorSpace1(p, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            if let Ok(vc2) = vc.cast::<ID3D11VideoContext2>() {
                match hdr10(color) {
                    Some(m) => vc2.VideoProcessorSetStreamHDRMetaData(
                        p,
                        0,
                        DXGI_HDR_METADATA_TYPE_HDR10,
                        size_of::<DXGI_HDR_METADATA_HDR10>() as u32,
                        Some((&raw const m).cast()),
                    ),
                    None => vc2.VideoProcessorSetStreamHDRMetaData(p, 0, DXGI_HDR_METADATA_TYPE_NONE, 0, None),
                }
                vc2.VideoProcessorSetOutputHDRMetaData(p, DXGI_HDR_METADATA_TYPE_NONE, 0, None);
            }
            let rect = |(w, h): (u32, u32)| RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            vc.VideoProcessorSetStreamSourceRect(p, 0, true, Some(&rect(self.src)));
            vc.VideoProcessorSetStreamDestRect(p, 0, true, Some(&rect(self.dst)));
            vc.VideoProcessorSetOutputTargetRect(p, true, Some(&rect(self.dst)));
            let mut stream =
                D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: ManuallyDrop::new(input), ..Default::default() };
            let blit = vc.VideoProcessorBlt(p, &self.output_view, 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            blit.map_err(err("blit"))?;
            self.context.CopyResource(&self.staging, &self.output);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(err("map"))?;
            let row = self.dst.0 as usize * 4;
            let (base, pitch) = (mapped.pData as *const u8, mapped.RowPitch as usize);
            let out = self.pool.fill(row * self.dst.1 as usize, |dst| {
                for (y, line) in dst.chunks_exact_mut(row).enumerate() {
                    line.copy_from_slice(std::slice::from_raw_parts(base.add(y * pitch), row));
                }
            });
            self.context.Unmap(&self.staging, 0);
            Ok(out)
        }
    }
}

/// The DXGI colour space of a picture with this description.
fn input_color_space(color: &ColorInfo) -> DXGI_COLOR_SPACE_TYPE {
    match (color.transfer, color.matrix, color.full_range) {
        (Transfer::Pq, ..) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
        (Transfer::Hlg, ..) => DXGI_COLOR_SPACE_YCBCR_STUDIO_GHLG_TOPLEFT_P2020,
        (Transfer::Sdr, ColorMatrix::Bt2020, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
        (Transfer::Sdr, ColorMatrix::Bt2020, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
        (Transfer::Sdr, ColorMatrix::Bt709, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
        (Transfer::Sdr, ColorMatrix::Bt709, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
        (Transfer::Sdr, ColorMatrix::Bt601, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
        (Transfer::Sdr, ColorMatrix::Bt601, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
    }
}

/// HDR10 metadata in DXGI's units: chromaticities × 50000, max luminance in nits, min in
/// 0.0001 nits. `None` for SDR or without metadata.
fn hdr10(color: &ColorInfo) -> Option<DXGI_HDR_METADATA_HDR10> {
    let hdr = color.hdr.filter(|_| color.transfer == Transfer::Pq)?;
    let xy = |(x, y): (f32, f32)| [(x * 50000.0).round() as u16, (y * 50000.0).round() as u16];
    let [r, g, b] = hdr.display_primaries.unwrap_or([(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)]);
    Some(DXGI_HDR_METADATA_HDR10 {
        RedPrimary: xy(r),
        GreenPrimary: xy(g),
        BluePrimary: xy(b),
        WhitePoint: xy(hdr.white_point.unwrap_or((0.3127, 0.3290))),
        MaxMasteringLuminance: hdr.max_luminance.unwrap_or(1000.0).round() as u32,
        MinMasteringLuminance: (hdr.min_luminance.unwrap_or(0.0) * 10000.0).round() as u32,
        MaxContentLightLevel: hdr.max_cll.unwrap_or(0),
        MaxFrameAverageLightLevel: hdr.max_fall.unwrap_or(0),
    })
}
