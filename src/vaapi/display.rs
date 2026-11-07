//! A VA-API display on an AMD or Intel GPU's render node, and what it decodes.

use std::ffi::CStr;
use std::os::fd::AsRawFd;

use super::sys::*;
use crate::{Error, Result};

/// An initialized VA display, closed when dropped.
pub struct Display {
    pub va: &'static Va,
    pub dpy: VADisplay,
    pub vendor: String,
    _node: std::fs::File,
}

// SAFETY: libva serializes calls on a display; each decoder uses its own.
unsafe impl Send for Display {}

unsafe extern "C" fn quiet(_: *mut core::ffi::c_void, _: *const core::ffi::c_char) {}

unsafe extern "C" fn to_log(_: *mut core::ffi::c_void, message: *const core::ffi::c_char) {
    if !message.is_null() {
        log::debug!("libva: {}", unsafe { CStr::from_ptr(message) }.to_string_lossy().trim_end());
    }
}

/// NVIDIA's VA-API driver (decoding through NVDEC) is left to the NVDEC backend, which does
/// more; `ALHAZEN_VAAPI_ANY_DRIVER=1` takes it anyway (to test this backend on NVIDIA).
fn wanted(vendor: &str) -> bool {
    let nvidia = vendor.contains("NVDEC") || vendor.to_ascii_lowercase().contains("nvidia");
    !nvidia || std::env::var_os("ALHAZEN_VAAPI_ANY_DRIVER").is_some_and(|v| v == "1")
}

impl Display {
    /// The first render node with an AMD or Intel driver.
    pub fn open() -> Result<Display> {
        let va = Va::get().map_err(|e| Error::Unsupported(if e.is_empty() { "VA-API" } else { "VA-API (libva not found)" }))?;
        let mut last = None;
        for n in 128..192 {
            let path = format!("/dev/dri/renderD{n}");
            let Ok(node) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
            match Display::on(va, node) {
                Ok(display) if wanted(&display.vendor) => return Ok(display),
                Ok(display) => log::debug!("VA-API: {path} is {} (left to NVDEC)", display.vendor),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(Error::Unsupported("VA-API (no AMD or Intel GPU)")))
    }

    fn on(va: &'static Va, node: std::fs::File) -> Result<Display> {
        // SAFETY: a render node we opened; libva keeps no other reference to the fd.
        let dpy = unsafe { (va.vaGetDisplayDRM)(node.as_raw_fd()) };
        if dpy.is_null() {
            return Err(Error::Unsupported("VA-API display"));
        }
        // libva prints its version to stderr on initialization unless told where to send it.
        unsafe {
            (va.vaSetInfoCallback)(dpy, Some(quiet), std::ptr::null_mut());
            (va.vaSetErrorCallback)(dpy, Some(to_log), std::ptr::null_mut());
        }
        let (mut major, mut minor) = (0, 0);
        let status = unsafe { (va.vaInitialize)(dpy, &mut major, &mut minor) };
        if status != VA_STATUS_SUCCESS as i32 {
            unsafe { (va.vaTerminate)(dpy) };
            return Err(Error::Decode(format!("VA-API: {}", va.error(status))));
        }
        let vendor = unsafe { (va.vaQueryVendorString)(dpy) };
        let vendor = if vendor.is_null() { String::new() } else { unsafe { CStr::from_ptr(vendor) }.to_string_lossy().into_owned() };
        Ok(Display { va, dpy, vendor, _node: node })
    }

    /// Fails with libva's words for `status`, unless it's success.
    pub fn check(&self, status: VAStatus, what: &str) -> Result<()> {
        if status == VA_STATUS_SUCCESS as i32 { Ok(()) } else { Err(Error::Decode(format!("VA-API {what}: {}", self.va.error(status)))) }
    }

    /// The profiles the GPU decodes (VLD).
    pub fn decode_profiles(&self) -> Vec<VAProfile> {
        let va = self.va;
        unsafe {
            let max = (va.vaMaxNumProfiles)(self.dpy).max(0) as usize;
            let mut profiles = vec![0 as VAProfile; max];
            let mut count = 0;
            if (va.vaQueryConfigProfiles)(self.dpy, profiles.as_mut_ptr(), &mut count) != VA_STATUS_SUCCESS as i32 {
                return Vec::new();
            }
            profiles.truncate(count.max(0) as usize);
            let max_entry = (va.vaMaxNumEntrypoints)(self.dpy).max(0) as usize;
            profiles
                .into_iter()
                .filter(|profile| {
                    let mut entries = vec![0 as VAEntrypoint; max_entry];
                    let mut n = 0;
                    (va.vaQueryConfigEntrypoints)(self.dpy, *profile, entries.as_mut_ptr(), &mut n) == VA_STATUS_SUCCESS as i32 && entries[..n.max(0) as usize].contains(&VAEntrypointVLD)
                })
                .collect()
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: initialized in `on`, terminated once.
        unsafe { (self.va.vaTerminate)(self.dpy) };
    }
}

/// What this machine's AMD or Intel GPU decodes, looked at once.
pub struct Caps {
    pub profiles: Vec<VAProfile>,
}

pub fn caps() -> Option<&'static Caps> {
    static CAPS: std::sync::OnceLock<Option<Caps>> = std::sync::OnceLock::new();
    CAPS.get_or_init(|| {
        let display = Display::open().ok()?;
        let profiles = display.decode_profiles();
        log::info!("VA-API: {} decodes {} profiles", display.vendor, profiles.len());
        Some(Caps { profiles })
    })
    .as_ref()
}

/// The VA profile to decode an H.264 SPS's profile with, if the GPU has one: the stream's own,
/// else a superset (Constrained Baseline and Main streams decode as High).
pub fn h264_profile(profile_idc: u32, constraint_flags: u32, have: &[VAProfile]) -> Option<VAProfile> {
    let wanted: &[VAProfile] = match profile_idc {
        // Baseline without its extras (FMO is refused earlier) decodes as Constrained Baseline.
        66 => &[VAProfileH264ConstrainedBaseline, VAProfileH264Main, VAProfileH264High],
        77 => &[VAProfileH264Main, VAProfileH264High],
        100 => &[VAProfileH264High],
        _ => &[],
    };
    let _ = constraint_flags;
    wanted.iter().copied().find(|p| have.contains(p))
}
