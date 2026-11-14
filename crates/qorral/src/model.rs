//! Turns the library on disk into what the UI shows, and holds the PoC
//! presets. Everything here may block: call it from worker threads.

use crate::ui::{RunState, Thumb, VmView};
use qorral_core::host::QemuInstall;
use qorral_core::supervisor::{self, State};
use qorral_core::{DiskBus, Firmware, GpuMode, Library, OsKind, VmConfig, VmEntry};
use std::path::Path;

/// Largest thumbnail kept in memory; the UI scales it down further.
const THUMB_MAX_W: usize = 960;

pub fn view(entry: &VmEntry, q: Option<&QemuInstall>) -> VmView {
    let c = &entry.config;
    let state = match (&entry.error, supervisor::state(entry)) {
        (Some(e), _) => RunState::Failed(e.clone()),
        (None, State::Running) => RunState::Running { since: entry.run.as_ref().map_or(0, |r| r.started) },
        (None, State::Stopped) => RunState::Off,
        (None, State::Exited) => RunState::Off,
    };
    VmView {
        id: entry.id.clone(),
        name: c.name.clone(),
        os: c.os,
        os_label: os_label(c),
        cpus: c.cpus,
        memory_mib: c.memory_mib,
        disk_gib: c.disk_gib,
        gpu: c.gpu,
        uefi: c.firmware == Firmware::Uefi,
        nvme: c.disk_bus == DiskBus::Nvme,
        forwards: c.port_forwards.len(),
        state,
        thumb: load_thumb(&entry.dir.join("thumb.ppm")),
        command: supervisor::command_line(q, entry),
    }
}

pub fn views(lib: &Library, q: Option<&QemuInstall>) -> Vec<VmView> {
    lib.list().iter().map(|e| view(e, q)).collect()
}

pub fn os_label(c: &VmConfig) -> String {
    if let Some(p) = c.preset.as_deref().and_then(preset) {
        return p.label.to_string();
    }
    match c.os {
        OsKind::Linux => "Linux",
        OsKind::Windows => "Windows",
        OsKind::Bsd => "BSD",
        OsKind::Other => "Other system",
    }
    .to_string()
}

/// PoC presets: sensible defaults per system. The OS catalog (HG-9) replaces
/// this table with versioned, verified download entries.
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub os: OsKind,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub disk_bus: DiskBus,
    pub gpu: GpuMode,
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        id: "ubuntu",
        label: "Ubuntu",
        os: OsKind::Linux,
        cpus: 4,
        memory_mib: 8192,
        disk_gib: 64,
        disk_bus: DiskBus::Virtio,
        gpu: GpuMode::Basic,
    },
    Preset {
        id: "fedora",
        label: "Fedora",
        os: OsKind::Linux,
        cpus: 4,
        memory_mib: 8192,
        disk_gib: 64,
        disk_bus: DiskBus::Virtio,
        gpu: GpuMode::Basic,
    },
    Preset {
        id: "debian",
        label: "Debian",
        os: OsKind::Linux,
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 32,
        disk_bus: DiskBus::Virtio,
        gpu: GpuMode::Basic,
    },
    Preset {
        id: "arch",
        label: "Arch Linux",
        os: OsKind::Linux,
        cpus: 4,
        memory_mib: 4096,
        disk_gib: 48,
        disk_bus: DiskBus::Virtio,
        gpu: GpuMode::Basic,
    },
    // NVMe: Windows setup has an inbox driver for it, not for virtio-blk.
    Preset {
        id: "windows-11",
        label: "Windows 11",
        os: OsKind::Windows,
        cpus: 4,
        memory_mib: 8192,
        disk_gib: 96,
        disk_bus: DiskBus::Nvme,
        gpu: GpuMode::Basic,
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// A new machine's settings from a preset (or plain defaults) and an image.
pub fn config_for(name: &str, preset_id: Option<&str>, iso: Option<&Path>) -> VmConfig {
    let mut c = VmConfig { name: name.to_string(), iso: iso.map(|p| p.display().to_string()), ..VmConfig::default() };
    if let Some(p) = preset_id.and_then(preset) {
        c.preset = Some(p.id.to_string());
        c.os = p.os;
        c.cpus = p.cpus;
        c.memory_mib = p.memory_mib;
        c.disk_gib = p.disk_gib;
        c.disk_bus = p.disk_bus;
        c.gpu = p.gpu;
    } else if let Some(guess) = iso.and_then(guess_preset) {
        return config_for(name, Some(guess), iso);
    }
    c
}

/// Guesses the system from the image's file name (`Fedora-Workstation-Live-…`).
pub fn guess_preset(iso: &Path) -> Option<&'static str> {
    let n = iso.file_name()?.to_string_lossy().to_lowercase();
    if n.contains("win11") || n.contains("windows") || n.starts_with("win") {
        Some("windows-11")
    } else {
        PRESETS.iter().map(|p| p.id).find(|id| n.contains(id.split('-').next().unwrap_or(id)))
    }
}

/// A friendly default name: the preset's label, or the image's file stem.
pub fn default_name(preset_id: Option<&str>, iso: Option<&Path>) -> String {
    if let Some(p) = preset_id.and_then(preset) {
        return p.label.to_string();
    }
    let stem = iso.and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let words: Vec<&str> = stem.split(['-', '_', '.', ' ']).filter(|w| !w.is_empty()).take(2).collect();
    if words.is_empty() { "New machine".into() } else { words.join(" ") }
}

/// Loads QEMU's screendump (PPM) as an RGBA thumbnail, scaled down to at
/// most `THUMB_MAX_W` wide.
pub fn load_thumb(path: &Path) -> Option<Thumb> {
    let img = qorral_core::image::parse_ppm(&std::fs::read(path).ok()?)?;
    let k = img.w.div_ceil(THUMB_MAX_W).max(1);
    let (tw, th) = (img.w / k, img.h / k);
    let mut rgba = Vec::with_capacity(tw * th * 4);
    for y in 0..th {
        for x in 0..tw {
            let p = (y * k * img.w + x * k) * 3;
            rgba.extend_from_slice(&[img.px[p], img.px[p + 1], img.px[p + 2], 255]);
        }
    }
    Some(Thumb { w: tw, h: th, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_follow_the_image_name() {
        let c = config_for("x", None, Some(Path::new("/i/Fedora-Workstation-Live-43.iso")));
        assert_eq!(c.preset.as_deref(), Some("fedora"));
        let w = config_for("w", None, Some(Path::new("Win11_25H2_English_x64.iso")));
        assert_eq!((w.os, w.disk_bus), (OsKind::Windows, DiskBus::Nvme));
        let other = config_for("o", None, Some(Path::new("/i/some-random-os.iso")));
        assert_eq!(other.preset, None);
        assert_eq!(default_name(None, Some(Path::new("/i/some-random-os.iso"))), "some random");
        assert_eq!(default_name(Some("arch"), None), "Arch Linux");
    }
}
