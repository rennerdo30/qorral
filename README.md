# Qorral

A fast virtual machine manager for Windows, Linux and macOS, built on QEMU.

> **Status: proof of concept.** Nothing is released yet. Expect breaking changes.

- **Every machine at a glance:** live guest screens, state and hardware on one screen.
- **Made for speed:** the window paints its first frame in about 20–30 ms, draws only what changed, and idles at zero CPU. A remote desktop mode switches to CPU rendering, no motion and damage-only updates.
- **GUI and CLI in one:** everything the app does, `qorral <command>` does too, with `--json` output and stable exit codes for scripts and other applications.
- **Planned:** an OS catalog with verified downloads, an unattended Windows install wizard, GPU-accelerated guests (OpenGL and Vulkan), disposable sandboxes, a background service, shared folders and app VMs.

## Try it

```sh
scripts/run.bat                 # Windows (scripts/run.sh on macOS and Linux)
scripts/snapshot.bat --demo     # render the UI offscreen to artifacts/snapshot.png
```

Qorral needs a QEMU build. Set `QORRAL_QEMU` to the folder that contains `qemu-system-x86_64`, or put it on `PATH`.

```sh
qorral host                                  # QEMU build, accelerator, library location
qorral create fedora --iso Fedora.iso        # disk, UEFI store and settings
qorral start fedora --window                 # or headless, with --minutes N as a time limit
qorral list --json
qorral screenshot fedora screen.png
qorral stop fedora                           # ACPI shutdown; --force ends QEMU at once
```

Exit codes: 0 done, 1 failed, 2 usage error, 3 no such machine.

## Build

See [docs/BUILD.md](docs/BUILD.md). The toolchain is pinned in `rust-toolchain.toml`.

## Licence

MIT. QEMU is a separate program under its own licence (GPL-2.0); Qorral starts it as a process and does not link it.
