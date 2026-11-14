//! Platform specifics: remote-session detection, cloaked windows for agent
//! smoke runs, the dark title bar and the system file picker.

/// True inside a remote desktop session: the UI switches to its remote mode
/// (CPU renderer, no motion, solid fills, damage-only presents).
#[cfg(windows)]
pub fn is_remote_session() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    if std::env::var_os("QORRAL_REMOTE").is_some() {
        return true;
    }
    // SAFETY: a plain metrics query.
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

#[cfg(not(windows))]
pub fn is_remote_session() -> bool {
    std::env::var_os("QORRAL_REMOTE").is_some()
}

/// Shows a window that DWM composes but never puts on screen (agent runs).
#[cfg(windows)]
pub fn cloak_and_show(hwnd: isize) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, ShowWindow};
    let on: i32 = 1;
    // SAFETY: hwnd is our own live window; the attribute value outlives the call.
    unsafe {
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        if DwmSetWindowAttribute(hwnd, DWMWA_CLOAK as u32, (&on as *const i32).cast(), 4) < 0 {
            return false;
        }
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    true
}

#[cfg(not(windows))]
pub fn cloak_and_show(_hwnd: isize) -> bool {
    false
}

/// Dark title bar on Windows, to match the dark theme.
#[cfg(windows)]
pub fn dark_title_bar(hwnd: isize) {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    if hwnd == 0 {
        return;
    }
    let on: i32 = 1;
    // SAFETY: hwnd is our own live window; the attribute value outlives the call.
    unsafe {
        DwmSetWindowAttribute(
            hwnd as windows_sys::Win32::Foundation::HWND,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&on as *const i32).cast(),
            4,
        );
    }
}

#[cfg(not(windows))]
pub fn dark_title_bar(_hwnd: isize) {}

/// Asks for a disk image with the system's own picker. Blocking: call it on
/// a worker thread. `None` when cancelled or when no picker is available.
pub fn pick_iso(title: &str) -> Option<String> {
    #[cfg(windows)]
    {
        win_pick::pick(title)
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!("POSIX path of (choose file with prompt \"{}\")", title.replace('"', "'"));
        let mut c = std::process::Command::new("osascript");
        c.args(["-e", &script]);
        let out = qorral_core::process::run_with_timeout(c, std::time::Duration::from_secs(3600)).ok()?;
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !p.is_empty()).then_some(p)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut c = std::process::Command::new("zenity");
        c.args(["--file-selection", "--title", title, "--file-filter=Disk images | *.iso *.img"]);
        let out = qorral_core::process::run_with_timeout(c, std::time::Duration::from_secs(3600)).ok()?;
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !p.is_empty()).then_some(p)
    }
}

#[cfg(windows)]
mod win_pick {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn pick(title: &str) -> Option<String> {
        // SAFETY: the dialog wants an apartment-threaded COM on this worker
        // thread; every buffer passed in outlives the blocking call.
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let title_w = wide(title);
            let mut buf = vec![0u16; 32 * 1024];
            let filter: Vec<u16> =
                "Disk images (*.iso, *.img)\0*.iso;*.img\0All files\0*.*\0\0".encode_utf16().collect();
            let mut of: OPENFILENAMEW = std::mem::zeroed();
            of.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
            of.lpstrFile = buf.as_mut_ptr();
            of.nMaxFile = buf.len() as u32;
            of.lpstrFilter = filter.as_ptr();
            of.nFilterIndex = 1;
            of.lpstrTitle = title_w.as_ptr();
            of.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
            if GetOpenFileNameW(&mut of) == 0 {
                return None;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..n])).filter(|p| !p.is_empty())
        }
    }
}
