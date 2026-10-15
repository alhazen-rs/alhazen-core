//! CoreFoundation helpers.

use std::ptr::NonNull;

use objc2_core_foundation::{CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use crate::{Error, Result};

/// A dictionary from string keys to values of any CoreFoundation type.
pub fn dictionary(entries: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary> {
    let keys: Vec<&CFString> = entries.iter().map(|(k, _)| *k).collect();
    let values: Vec<&CFType> = entries.iter().map(|(_, v)| *v).collect();
    let d = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    // SAFETY: a typed dictionary is an untyped one.
    unsafe { CFRetained::cast_unchecked(d) }
}

pub fn number(v: i64) -> CFRetained<CFNumber> {
    CFNumber::new_i64(v)
}

pub fn data(bytes: &[u8]) -> CFRetained<CFData> {
    CFData::from_bytes(bytes)
}

pub fn string(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

/// Takes ownership of an object a CoreFoundation-style "Create" function returned through an
/// out-pointer, after checking its status.
///
/// # Safety
/// `ptr` must be null or an object the caller owns (+1 retain count) of type `T`.
pub unsafe fn created<T: objc2_core_foundation::Type>(status: i32, ptr: *const T, what: &str) -> Result<CFRetained<T>> {
    if status != 0 {
        return Err(Error::Decode(format!("{what}: OSStatus {status}")));
    }
    let ptr = NonNull::new(ptr as *mut T).ok_or_else(|| Error::Decode(format!("{what}: no object")))?;
    // SAFETY: owned +1 per the caller's contract.
    Ok(unsafe { CFRetained::from_raw(ptr) })
}
