//! Turns a [`VmConfig`] into a QEMU command line.
//!
//! The argument sets come from the renner.dev OS lab tool, where each one was
//! tested on this QEMU build. One pure function, so every mode is unit-tested
//! without starting anything.

use crate::vm::{DiskBus, Firmware, GpuMode, NetworkMode, VmConfig};
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

    /// The machine's files, honouring an adopted disk or UEFI store elsewhere.
    pub fn for_vm(dir: &Path, vm: &VmConfig) -> VmPaths {
        let mut p = VmPaths::in_dir(dir);
        if let Some(d) = &vm.disk_path {
            p.disk = PathBuf::from(d);
        }
        if let Some(e) = &vm.efivars_path {
            p.efivars = PathBuf::from(e);
        }
        p
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
    /// No window. Qorral shows the guest through QMP screenshots (later its
    /// own D-Bus display client, HG-14).
    Headless,
    /// QEMU's own window.
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

/// The arguments after the executable. QEMU opens a window only when
/// `Display::Window` is asked for; Qorral never opens one on its own.
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
    push(&["-nic", &nic_arg(vm)]);
    push(&["-audiodev", "none,id=snd0"]);
    // A guest-triggered warm reset hangs under WHPX: a guest reboot ends
    // QEMU instead, and Qorral cold-boots the VM again.
    push(&["-no-reboot"]);
    push(&["-qmp", &format!("tcp:127.0.0.1:{},server=on,wait=off", l.ports.qmp)]);
    push(&["-device", "virtio-serial-pci"]);
    push(&["-chardev", &format!("socket,id=qga,host=127.0.0.1,port={},server=on,wait=off", l.ports.agent)]);
    push(&["-device", "virtserialport,chardev=qga,name=org.qemu.guest_agent.0"]);
    push(&["-serial", &format!("file:{}", l.paths.serial_log.display())]);
    let display = match (l.display, vm.gpu.accelerated()) {
        (Display::Window, true) => "sdl,gl=on",
        (Display::Window, false) => "sdl",
        // GL without a window needs EGL (`QemuInstall::has_egl`; the
        // supervisor checks it). On Windows the D-Bus display with GL: the
        // tested path (Venus desktop, 2026-11-15) and the one Qorral's own
        // display client will attach to. p2p: no session bus needed.
        (Display::Headless, true) if cfg!(windows) => "dbus,p2p=yes,gl=on",
        (Display::Headless, true) => "egl-headless",
        (Display::Headless, false) => "none",
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

/// The `-nic` value for the machine's network mode and forwards.
pub fn nic_arg(vm: &VmConfig) -> String {
    let mode = match vm.network {
        NetworkMode::None => return "none".into(),
        NetworkMode::Isolated => ",restrict=on",
        // Our QEMU fork's option (libslirp `disable_host_loopback`).
        NetworkMode::Internet => ",host-loopback=off",
        NetworkMode::Nat => "",
    };
    format!("user,model=virtio-net-pci{mode}")
        + &vm.port_forwards.iter().map(|f| format!(",hostfwd={f}")).collect::<String>()
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
    fn gpu_modes_never_open_a_window_unasked_and_windows_guests_get_nvme() {
        let vm = VmConfig { gpu: GpuMode::Vulkan, disk_bus: DiskBus::Nvme, ..VmConfig::default() };
        let a = launch_args(&vm, Display::Headless);
        assert!(has_pair(&a, "-device", "virtio-vga-gl,blob=on,hostmem=4G,venus=on"));
        assert!(!a.iter().any(|x| x.starts_with("sdl")), "headless must never open a window");
        let headless = if cfg!(windows) { "dbus,p2p=yes,gl=on" } else { "egl-headless" };
        assert!(has_pair(&a, "-display", headless));
        let w = launch_args(&vm, Display::Window);
        assert!(has_pair(&w, "-display", "sdl,gl=on"));
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
    fn network_modes_map_to_nic_options() {
        let fwd = vec!["tcp:127.0.0.1:2222-:22".to_string()];
        let nic = |network| nic_arg(&VmConfig { network, port_forwards: fwd.clone(), ..VmConfig::default() });
        assert_eq!(nic(NetworkMode::None), "none", "no card means no forwards either");
        assert_eq!(nic(NetworkMode::Isolated), "user,model=virtio-net-pci,restrict=on,hostfwd=tcp:127.0.0.1:2222-:22");
        assert_eq!(
            nic(NetworkMode::Internet),
            "user,model=virtio-net-pci,host-loopback=off,hostfwd=tcp:127.0.0.1:2222-:22"
        );
        assert_eq!(nic(NetworkMode::Nat), "user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:2222-:22");
    }

    #[test]
    fn adopted_disks_are_used_in_place() {
        let vm = VmConfig { disk_path: Some("/lab/vms/win/disk.qcow2".into()), ..VmConfig::default() };
        let paths = VmPaths::for_vm(Path::new("/q/vms/win"), &vm);
        assert_eq!(paths.disk, PathBuf::from("/lab/vms/win/disk.qcow2"));
        assert_eq!(paths.efivars, PathBuf::from("/q/vms/win/efivars.qcow2"));
    }

    #[test]
    fn sanitize_never_returns_empty() {
        assert_eq!(sanitize("  ***  "), "vm");
        assert_eq!(sanitize("Win 11 Pro"), "win-11-pro");
    }
}
