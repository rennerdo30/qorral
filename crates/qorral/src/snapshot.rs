//! Offscreen rendering of the real UI into a PNG: same state, same paint
//! code as the window, no window and no desktop disturbance.

use crate::ui::{HostInfo, RunState, Thumb, Ui, VmView, load_fonts};
use crate::{GuiArgs, model, trace};
use qorral_core::host::QemuInstall;
use qorral_core::{GpuMode, Library, OsKind};
use qorral_render::{Canvas, TextEngine};
use std::time::Instant;

pub fn run(args: &GuiArgs, t0: Instant) -> i32 {
    let Some(out) = &args.snapshot else { return 2 };
    let mut te = TextEngine::new();
    let Some(fonts) = load_fonts(&mut te) else {
        eprintln!("qorral: no usable UI font found");
        return 1;
    };
    let (w, h) = args.size;
    let scale = args.scale.unwrap_or(1.0);
    let mut ui = Ui::new(te, fonts, w, h, scale);
    ui.remote = args.remote;
    ui.now = qorral_core::supervisor::now();
    if args.demo {
        ui.host =
            HostInfo { accel: qorral_core::qemu::host_accel(), qemu_version: Some("11.1.90".into()), qemu_found: true };
        ui.set_vms(demo(ui.now));
    } else {
        let q = QemuInstall::find();
        ui.host = HostInfo {
            accel: qorral_core::qemu::host_accel(),
            qemu_version: q.as_ref().and_then(|q| q.version()),
            qemu_found: q.is_some(),
        };
        if !args.empty {
            ui.set_vms(model::views(&Library::new(Library::default_root()), q.as_ref()));
        }
    }
    if let Some(id) = &args.select {
        ui.selected = Some(id.clone());
        ui.set_vms(ui.vms.clone());
    }
    if let Some(q) = &args.query {
        ui.text(q);
    }
    if let Some((x, y)) = args.hover {
        ui.pointer_move(x, y);
    }
    trace(t0, "ui built");
    let mut px = vec![0u32; w * h];
    let mut cv = Canvas::new(&mut px, w, h);
    ui.paint(&mut cv, true);
    trace(t0, &format!("painted in {:.2} ms", ui.stats.last_paint_ms));
    let mut rgba = Vec::with_capacity(w * h * 4);
    for p in &px {
        rgba.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255]);
    }
    match write_png(out, &rgba, w, h) {
        Ok(()) => {
            trace(t0, "png written");
            0
        }
        Err(e) => {
            eprintln!("qorral: cannot write {}: {e}", out.display());
            1
        }
    }
}

pub fn write_png(path: &std::path::Path, rgba: &[u8], w: usize, h: usize) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(rgba).map_err(|e| e.to_string())
}

/// Example machines for UI review snapshots (`--demo`). The guest screens are
/// drawn here, not captured: they stand in for live screens in layout checks.
pub fn demo(now: u64) -> Vec<VmView> {
    let vm = |id: &str, name: &str, os: OsKind, label: &str, cpus, mem, disk, gpu, state, thumb| VmView {
        id: id.into(),
        name: name.into(),
        os,
        os_label: label.into(),
        cpus,
        memory_mib: mem,
        disk_gib: disk,
        gpu,
        uefi: true,
        nvme: os == OsKind::Windows,
        forwards: if id == "fedora" { 1 } else { 0 },
        state,
        thumb,
        command: format!(
            "qemu-system-x86_64 -name qorral-{id} -machine q35 -accel whpx -cpu host -smp {cpus} -m {mem}M -drive if=none,id=hd0,format=qcow2,file=vms/{id}/disk.qcow2 -device virtio-blk-pci,drive=hd0,bootindex=1 -device {} -device qemu-xhci -device usb-tablet -nic user,model=virtio-net-pci -no-reboot",
            if gpu == GpuMode::Vulkan { "virtio-vga-gl,blob=on,hostmem=4G,venus=on" } else { "virtio-vga" }
        ),
    };
    vec![
        vm(
            "fedora",
            "Fedora Workstation",
            OsKind::Linux,
            "Fedora",
            8,
            16384,
            128,
            GpuMode::Vulkan,
            RunState::Running { since: now - 4320 },
            Some(desktop(0x1E2A4A, 0x3B5BA5, false)),
        ),
        vm(
            "win11-dev",
            "Windows 11 Dev",
            OsKind::Windows,
            "Windows 11",
            6,
            12288,
            96,
            GpuMode::Basic,
            RunState::Running { since: now - 600 },
            Some(desktop(0x0E3B6E, 0x2A7BD1, true)),
        ),
        vm("arch", "Arch Linux", OsKind::Linux, "Arch Linux", 4, 4096, 48, GpuMode::Opengl, RunState::Off, None),
        vm(
            "debian-srv",
            "Debian build server",
            OsKind::Linux,
            "Debian",
            2,
            4096,
            32,
            GpuMode::Basic,
            RunState::Off,
            Some(console()),
        ),
        vm(
            "ubuntu",
            "Ubuntu",
            OsKind::Linux,
            "Ubuntu",
            4,
            8192,
            64,
            GpuMode::Basic,
            RunState::Failed("QEMU exited at start: Failed to find an available port".into()),
            None,
        ),
    ]
}

fn desktop(top: u32, bottom: u32, taskbar: bool) -> Thumb {
    let (w, h) = (640usize, 400usize);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        let c = qorral_render::mix(top, bottom, (y * 255 / h) as u32);
        for x in 0..w {
            let i = (y * w + x) * 4;
            let in_win = (120..520).contains(&x) && (70..300).contains(&y);
            let bar = if taskbar { y >= h - 26 } else { y < 18 };
            let col = if bar {
                0x1A1C22
            } else if in_win {
                if y < 92 { 0x2B2F38 } else { 0xF2F3F5 }
            } else {
                c
            };
            rgba[i..i + 4].copy_from_slice(&[(col >> 16) as u8, (col >> 8) as u8, col as u8, 255]);
        }
    }
    Thumb { w, h, rgba }
}

fn console() -> Thumb {
    let (w, h) = (640usize, 400usize);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let line = y / 14;
            let len = 120 + (line * 97 % 380);
            let text = y % 14 > 3 && y % 14 < 11 && x > 10 && x < len && line < 24 && (x / 7) % 9 != 0;
            let v = if text { 0xB8 } else { 0x08 };
            rgba[i..i + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    Thumb { w, h, rgba }
}
