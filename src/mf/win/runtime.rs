//! COM and Media Foundation start-up.

use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

use crate::{Error, Result};

/// Media Foundation is started once for the process and never shut down (shutting down while
/// another player still decodes would break it; the OS cleans up at exit).
pub fn ensure_started() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        // SAFETY: plain FFI call with constant arguments.
        .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| Error::Decode(format!("Media Foundation unavailable: {e}")))
}

/// Joins this thread to the multithreaded apartment. Our pipeline threads are ours, so this is
/// always possible there; if the thread already chose a single-threaded apartment (a caller's
/// thread), COM objects still work through marshalling and the error is ignored.
pub fn com_init() {
    // SAFETY: plain FFI call; balancing CoUninitialize is skipped deliberately (the thread
    // keeps its apartment for its lifetime).
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}
