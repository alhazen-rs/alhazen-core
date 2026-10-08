//! The shared D3D11 device Media Foundation's decoders use for GPU (DXVA) decoding, and which
//! codec profiles that GPU can decode.

use std::collections::HashSet;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11VideoDevice,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::core::{GUID, Interface};

use super::super::select::MfCodec;

/// The device, its Media Foundation device manager, and the GPU's decoder profiles.
pub struct Gpu {
    pub manager: IMFDXGIDeviceManager,
    profiles: HashSet<u128>,
    _device: ID3D11Device,
}

// SAFETY: the device is created with multithread protection (`ID3D11Multithread`), and the
// DXGI device manager is free-threaded by design (it hands out locked device access); both are
// documented as usable from any thread.
unsafe impl Send for Gpu {}
// SAFETY: as above.
unsafe impl Sync for Gpu {}

/// The process-wide GPU device, or `None` without a D3D11 hardware device with video support.
pub fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| create().map_err(|e| log::info!("no D3D11 video device: {e}")).ok()).as_ref()
}

fn create() -> windows::core::Result<Gpu> {
    let mut device = None;
    // SAFETY: FFI with valid out-pointers; the result is checked.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device: ID3D11Device = device.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
    // SAFETY: COM calls on a live device.
    unsafe {
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        let video: ID3D11VideoDevice = device.cast()?;
        let mut profiles = HashSet::new();
        for i in 0..video.GetVideoDecoderProfileCount() {
            if let Ok(g) = video.GetVideoDecoderProfile(i) {
                profiles.insert(g.to_u128());
            }
        }
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
        manager.ResetDevice(&device, token)?;
        Ok(Gpu { manager, profiles, _device: device })
    }
}

impl Gpu {
    /// Whether the GPU decodes `codec` (its main DXVA profile).
    pub fn decodes(&self, codec: MfCodec) -> bool {
        profiles(codec).iter().any(|p| self.profiles.contains(&p.to_u128()))
    }
}

/// DXVA decoder profile GUIDs per codec (any one suffices).
fn profiles(codec: MfCodec) -> &'static [GUID] {
    use windows::Win32::Graphics::Direct3D11::*;
    match codec {
        MfCodec::H264 => &[D3D11_DECODER_PROFILE_H264_VLD_NOFGT, D3D11_DECODER_PROFILE_H264_VLD_FGT],
        MfCodec::Hevc => &[D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10],
        MfCodec::Vp9 => &[D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0, D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2],
        MfCodec::Av1 => &[D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0],
        _ => &[],
    }
}
