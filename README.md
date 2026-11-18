# Qorral

A fast virtual machine manager for Windows, Linux and macOS, built on QEMU.

> **Status: proof of concept.** Nothing is released yet. Expect breaking changes.

- **Every machine at a glance:** live guest screens, state and hardware on one screen.
- **Made for speed:** the window paints its first frame in about 20–30 ms, draws only what changed, and idles at zero CPU. A remote desktop mode switches to CPU rendering, no motion and damage-only updates.
- **GUI and CLI in one:** everything the app does, `qorral <command>` does too, with `--json` output and stable exit codes for scripts and other applications.
- **A console fast enough for games:** a running machine's screen goes from the guest's GPU memory to the window with no copy, and presents the moment the guest flips. Over remote desktop it switches to shared memory and the CPU.
- **Planned:** an OS catalog with verified downloads, an unattended Windows install wizard, disposable sandboxes, a background service, shared folders and app VMs.

## Display latency

The time from a guest frame reaching Qorral to that frame being on screen, measured per frame with `QORRAL_TRACE=1`. The setup: a Windows 11 guest at 1920×1080 on a current desktop GPU, with a full-screen GPU benchmark running in the guest unless noted otherwise.

| Path | Median | 99th percentile |
|---|---|---|
| Zero copy, composed on a compute queue (default) | 0.6–1.3 ms | 4.6–7.5 ms |
| Zero copy, composed on the graphics queue | 1.5 ms | 7.3 ms |
| Zero copy, 1280×800, lighter guest scene (graphics queue) | 0.34 ms | 2.1 ms |
| Remote desktop: shared memory, drawn on the CPU (about 100 frames/s) | 1.6–1.9 ms | 4.3–5.4 ms |

The targets are under 10 ms always, under 5 ms typically, and 1 ms as the stretch goal. Every path stays under 10 ms. The slowest frames come from the guest's own GPU work competing for the GPU. QEMU adds about 0.5–0.7 ms from the guest's flip to Qorral, and input reaches the guest in about 0.2 ms.

End to end, a key press currently takes about 15 ms to show (median; Windows 11 guest with our guest display driver changes, which are not released yet). Nearly all of it is inside the guest: the application picks up the key at its next frame, then its present and the desktop compositor's present each spend about 3 ms in the guest's Direct3D 11 driver before the frame is flipped. From the flip to Qorral takes under 1 ms. Shortening those presents is the next step.

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
