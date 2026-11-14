//! Turns a [`VmConfig`] into a QEMU command line.
//!
//! The argument sets come from the renner.dev OS lab tool, where each one was
//! tested on this QEMU build. One pure function, so every mode is unit-tested
//! without starting anything.

use crate::vm::{DiskBus, Firmware, GpuMode, VmConfig};
use std::path::{Path, PathBuf};

/// Hardware acceleration per host OS.
pub fn host_accel() -> &'static str {
    if cfg!(windows) {
        "whpx"
    } else if cfg!(target_os = "macos") {
        "hvf"
    } else {
        "kvm"
    }
}

/// Where a VM's files are.
#[derive(Clone, Debug)]
pub struct VmPaths {
    pub disk: PathBuf,
    /// Per-VM UEFI variable store (qcow2, so disk snapshots include it).
    pub efivars: PathBuf,
    pub serial_log: PathBuf,
}

impl VmPaths {
    pub fn in_dir(dir: &Path) -> VmPaths {
        VmPaths { disk: dir.join("disk.qcow2"), efivars: dir.join("efivars.qcow2"), serial_log: dir.join("serial.log") }
    }
}

/// Loopback ports for one running VM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub qmp: u16,
    /// QEMU guest agent (commands inside installed guests).
    pub agent: u16,
}

/// How the guest screen is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    /// No window. Qorral shows the guest through QMP screenshots.
    Headless,
    /// QEMU's own window (the only path that shows accelerated guests today).
    Window,
}

#[derive(Clone, Debug)]
pub struct Launch<'a> {
    pub vm: &'a VmConfig,
    pub paths: &'a VmPaths,
    pub ports: Ports,
    pub display: Display,
    pub accel: &'a str,
    /// UEFI firmware code (read-only), e.g. `share/edk2-x86_64-code.fd`.
    pub firmware_code: Option<&'a Path>,
}

/// The arguments after the executable. Accelerated graphics need a window
/// (they render through its GL context), so `Headless` with an accelerated
/// GPU mode falls back to a window; the caller learns that from
/// [`effective_display`].
pub fn args(l: &Launch) -> Vec<String> {
    let vm = l.vm;
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    let name = format!("qorral-{}", sanitize(&vm.name));
    push(&["-name", &name]);
    push(&["-machine", "q35", "-accel", l.accel, "-cpu", "host"]);
    push(&["-smp", &vm.cpus.max(1).to_string(), "-m", &format!("{}M", vm.memory_mib.max(256))]);
    push(&["-drive", &format!("if=none,id=hd0,format=qcow2,file={}", l.paths.disk.display())]);
    push(&[
        "-device",
        match vm.disk_bus {
            DiskBus::Nvme => "nvme,drive=hd0,serial=qorral-hd0,bootindex=1",
            DiskBus::Virtio => "virtio-blk-pci,drive=hd0,bootindex=1",
        },
    ]);
    push(&[
        "-device",
        match vm.gpu {
            GpuMode::Basic => "virtio-vga",
            GpuMode::Opengl => "virtio-vga-gl",
            GpuMode::Vulkan => "virtio-vga-gl,blob=on,hostmem=4G,venus=on",
        },
    ]);
    push(&["-device", "qemu-xhci", "-device", "usb-tablet"]);
    let nic: String = "user,model=virtio-net-pci".to_string()
        + &vm.port_forwards.iter().map(|f| format!(",hostfwd={f}")).collect::<String>();
    push(&["-nic", &nic]);
    push(&["-audiodev", "none,id=snd0"]);
    // A guest-triggered warm reset hangs under WHPX: a guest reboot ends
    // QEMU instead, and Qorral cold-boots the VM again.
    push(&["-no-reboot"]);
    push(&["-qmp", &format!("tcp:127.0.0.1:{},server=on,wait=off", l.ports.qmp)]);
    push(&["-device", "virtio-serial-pci"]);
    push(&["-chardev", &format!("socket,id=qga,host=127.0.0.1,port={},server=on,wait=off", l.ports.agent)]);
    push(&["-device", "virtserialport,chardev=qga,name=org.qemu.guest_agent.0"]);
    push(&["-serial", &format!("file:{}", l.paths.serial_log.display())]);
    let display = match (effective_display(l), vm.gpu.accelerated()) {
        (Display::Window, true) => "sdl,gl=on",
        (Display::Window, false) => "sdl",
        (Display::Headless, _) => "none",
    };
    push(&["-display", display]);
    if vm.firmware == Firmware::Uefi
        && let Some(code) = l.firmware_code
    {
        push(&["-drive", &format!("if=pflash,format=raw,readonly=on,file={}", code.display())]);
        push(&["-drive", &format!("if=pflash,format=qcow2,file={}", l.paths.efivars.display())]);
    }
    if let Some(iso) = &vm.iso {
        // bootindex, not `-boot once=d`: UEFI keeps booting an installed disk first otherwise.
        push(&["-drive", &format!("if=none,id=cd0,media=cdrom,format=raw,readonly=on,file={iso}")]);
        push(&["-device", "ide-cd,bus=ide.0,drive=cd0,bootindex=0"]);
    }
    a.extend(vm.extra_args.iter().cloned());
    a
}

pub fn effective_display(l: &Launch) -> Display {
    if l.vm.gpu.accelerated() { Display::Window } else { l.display }
}

/// QEMU's `-name` and our folder names: letters, digits, `-` and `_` only.
pub fn sanitize(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "vm".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_args(vm: &VmConfig, display: Display) -> Vec<String> {
        let paths = VmPaths::in_dir(Path::new("/vms/a"));
        let fw = PathBuf::from("/q/share/edk2-x86_64-code.fd");
        args(&Launch {
            vm,
            paths: &paths,
            ports: Ports { qmp: 44400, agent: 44600 },
            display,
            accel: "whpx",
            firmware_code: Some(&fw),
        })
    }

    fn has_pair(a: &[String], flag: &str, value: &str) -> bool {
        a.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn linux_defaults_are_headless_virtio_uefi() {
        let vm = VmConfig { name: "Fedora 43!".into(), ..VmConfig::default() };
        let a = launch_args(&vm, Display::Headless);
        assert!(has_pair(&a, "-name", "qorral-fedora-43"));
        assert!(has_pair(&a, "-device", "virtio-blk-pci,drive=hd0,bootindex=1"));
        assert!(has_pair(&a, "-device", "virtio-vga"));
        assert!(has_pair(&a, "-display", "none"));
        assert!(has_pair(&a, "-m", "8192M"));
        assert!(a.iter().any(|s| s == "-no-reboot"));
        assert!(a.iter().any(|s| s.starts_with("if=pflash,format=qcow2,file=")));
        assert!(has_pair(&a, "-qmp", "tcp:127.0.0.1:44400,server=on,wait=off"));
    }

    #[test]
    fn vulkan_forces_a_gl_window_and_windows_guests_get_nvme() {
        let vm = VmConfig { gpu: GpuMode::Vulkan, disk_bus: DiskBus::Nvme, ..VmConfig::default() };
        let a = launch_args(&vm, Display::Headless);
        assert!(has_pair(&a, "-device", "virtio-vga-gl,blob=on,hostmem=4G,venus=on"));
        assert!(has_pair(&a, "-display", "sdl,gl=on"));
        assert!(has_pair(&a, "-device", "nvme,drive=hd0,serial=qorral-hd0,bootindex=1"));
    }

    #[test]
    fn iso_boots_first_bios_skips_pflash_and_extras_come_last() {
        let vm = VmConfig {
            firmware: Firmware::Bios,
            iso: Some("/isos/x.iso".into()),
            port_forwards: vec!["tcp:127.0.0.1:2222-:22".into()],
            extra_args: vec!["-rtc".into(), "base=localtime".into()],
            ..VmConfig::default()
        };
        let a = launch_args(&vm, Display::Window);
        assert!(has_pair(&a, "-device", "ide-cd,bus=ide.0,drive=cd0,bootindex=0"));
        assert!(!a.iter().any(|s| s.contains("pflash")));
        assert!(has_pair(&a, "-nic", "user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:2222-:22"));
        assert!(has_pair(&a, "-display", "sdl"));
        assert_eq!(&a[a.len() - 2..], ["-rtc", "base=localtime"]);
    }

    #[test]
    fn sanitize_never_returns_empty() {
        assert_eq!(sanitize("  ***  "), "vm");
        assert_eq!(sanitize("Win 11 Pro"), "win-11-pro");
    }
}
