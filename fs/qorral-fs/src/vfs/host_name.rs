// qorral: the real on-disk name of a host path, for the exact-case check.

use std::path::Path;

/// The name the host file system stores for the last component of `path`
/// (with its real case), or `None` when it cannot be read.
#[cfg(windows)]
pub(crate) fn actual_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, WIN32_FIND_DATAW,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // A wildcard would match other entries; such names are never checked.
    if wide.iter().any(|&c| c == u16::from(b'*') || c == u16::from(b'?')) {
        return None;
    }
    // SAFETY: zeroed POD out-struct; NUL-terminated path; the handle is closed.
    unsafe {
        let mut data: WIN32_FIND_DATAW = std::mem::zeroed();
        let h = FindFirstFileExW(
            wide.as_ptr(),
            FindExInfoBasic,
            (&mut data as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            0,
        );
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        FindClose(h);
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
        Some(String::from_utf16_lossy(&data.cFileName[..len]))
    }
}

/// Case-sensitive hosts: the requested name is the stored name.
#[cfg(not(windows))]
pub(crate) fn actual_name(_path: &Path) -> Option<String> {
    None
}
