//! The QEMU installation Qorral drives, and what it can do.

use crate::process::run_with_timeout;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct QemuInstall {
    /// Folder with `qemu-system-x86_64` and `qemu-img`.
    pub dir: PathBuf,
    /// Folder with the firmware (`share/`).
    pub share: PathBuf,
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

impl QemuInstall {
    /// Looks in order at: `QORRAL_QEMU`, a `qemu` folder next to Qorral's own
    /// executable (the packaged layout), then every folder on `PATH`.
    pub fn find() -> Option<QemuInstall> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(d) = std::env::var_os("QORRAL_QEMU") {
            candidates.push(PathBuf::from(d));
        }
        if let Ok(me) = std::env::current_exe()
            && let Some(d) = me.parent()
        {
            candidates.push(d.join("qemu"));
        }
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path));
        }
        candidates.into_iter().find_map(|d| Self::at(&d))
    }

    pub fn at(dir: &Path) -> Option<QemuInstall> {
        if !dir.join(exe("qemu-system-x86_64")).is_file() {
            return None;
        }
        let share = [dir.join("share"), dir.join("../share/qemu")].into_iter().find(|s| s.is_dir())?;
        Some(QemuInstall { dir: dir.to_path_buf(), share })
    }

    pub fn system(&self) -> PathBuf {
        self.dir.join(exe("qemu-system-x86_64"))
    }
    pub fn img(&self) -> PathBuf {
        self.dir.join(exe("qemu-img"))
    }
    pub fn firmware_code(&self) -> Option<PathBuf> {
        Some(self.share.join("edk2-x86_64-code.fd")).filter(|p| p.is_file())
    }
    /// The UEFI variable store template, copied per VM.
    pub fn firmware_vars(&self) -> Option<PathBuf> {
        ["edk2-i386-vars.fd", "edk2-x86_64-vars.fd"].iter().map(|n| self.share.join(n)).find(|p| p.is_file())
    }

    /// Whether windowless GL displays can work. On Windows they need an EGL
    /// library next to QEMU (ANGLE's `libEGL.dll` + `libGLESv2.dll`); without
    /// one, older builds crash at start. Elsewhere the system provides EGL.
    pub fn has_egl(&self) -> bool {
        if cfg!(windows) {
            self.dir.join("libEGL.dll").is_file() && self.dir.join("libGLESv2.dll").is_file()
        } else {
            true
        }
    }

    /// Whether this build can share host folders (our virtio-fs device and
    /// its file-server library next to QEMU).
    pub fn has_fs_server(&self) -> bool {
        let lib = if cfg!(windows) {
            "qorral_fs.dll"
        } else if cfg!(target_os = "macos") {
            "libqorral_fs.dylib"
        } else {
            "libqorral_fs.so"
        };
        self.dir.join(lib).is_file()
    }

    /// `QEMU emulator version 11.1.90` → `11.1.90`.
    pub fn version(&self) -> Option<String> {
        let mut c = Command::new(self.system());
        c.arg("--version");
        let out = run_with_timeout(c, Duration::from_secs(20)).ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        parse_version(&text)
    }

    /// Display backends this build offers (`-display help`).
    pub fn displays(&self) -> Vec<String> {
        let mut c = Command::new(self.system());
        c.args(["-display", "help"]);
        match run_with_timeout(c, Duration::from_secs(20)) {
            Ok(out) => parse_displays(&String::from_utf8_lossy(&out.stdout)),
            Err(_) => Vec::new(),
        }
    }
}

/// The refresh rate (Hz) a guest display reports: `QORRAL_REFRESH_HZ` when set, else the highest
/// among the monitors attached to the desktop, if known.
pub fn max_refresh_hz() -> Option<u32> {
    std::env::var("QORRAL_REFRESH_HZ")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|hz| (24..=1000).contains(hz))
        .or_else(monitor_refresh_hz)
}

#[cfg(windows)]
fn monitor_refresh_hz() -> Option<u32> {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW,
        EnumDisplaySettingsW,
    };
    let mut best = None;
    for i in 0.. {
        // SAFETY: zeroed out-structs with their size set, as the API requires.
        let mut dev: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dev.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
        if unsafe { EnumDisplayDevicesW(std::ptr::null(), i, &mut dev, 0) } == 0 {
            break;
        }
        if dev.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP == 0 {
            continue;
        }
        let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        // SAFETY: the device name comes from EnumDisplayDevicesW (null-terminated).
        if unsafe { EnumDisplaySettingsW(dev.DeviceName.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) } != 0
            && mode.dmDisplayFrequency > 1
        {
            best = best.max(Some(mode.dmDisplayFrequency));
        }
    }
    best
}

#[cfg(not(windows))]
fn monitor_refresh_hz() -> Option<u32> {
    None
}

pub fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains("version"))?;
    line.split_whitespace().skip_while(|w| *w != "version").nth(1).map(|v| v.trim_end_matches(',').to_string())
}

pub fn parse_displays(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !l.to_ascii_lowercase().contains("display backend"))
        .skip(1)
        .map(str::trim)
        .take_while(|l| !l.is_empty())
        .filter(|l| l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_and_display_list() {
        assert_eq!(parse_version("QEMU emulator version 11.1.90\nCopyright (c) 2003-2026").as_deref(), Some("11.1.90"));
        assert_eq!(parse_version("garbage"), None);
        let help =
            "Available display backend types:\nnone\nsdl\negl-headless\ncurses\ndbus\n\nSome display backends support";
        assert_eq!(parse_displays(help), ["none", "sdl", "egl-headless", "curses", "dbus"]);
    }
}
