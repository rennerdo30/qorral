# Building Qorral

```sh
cargo run -p qorral --bin qorral-gui                     # the app
cargo run -p qorral -- list                              # the CLI
cargo run -p qorral --bin qorral-gui -- --snapshot out.png --demo   # offscreen render
QORRAL_TRACE=1 cargo run --release -p qorral --bin qorral-gui       # startup and present marks
```

## Layout

| Crate | What |
|---|---|
| `qorral-core` | VM model (`vm.json`), QEMU command line, QMP client, library, supervisor (start, stop, PIDs). No UI. |
| `qorral-render` | CPU canvas and text (anti-aliased shapes, fontdue glyph cache). No windowing. |
| `qorral` | `qorral` (console CLI) and `qorral-gui` (window: winit + softbuffer, damage-only presents). |

## Fastest iteration

- `.cargo/config.toml` is machine-local and gitignored. `scripts/_setup.bat` copies it from `.cargo/config.example.toml` on the first run. It sets a shared sccache, `rust-lld` on Windows and a build folder outside the repository.
- Dev profile: line tables only; dependencies and the renderer are optimised.

## Measured (owner PC, Windows 11, 2026-11-15)

| Measure | Result |
|---|---|
| Launch to first frame (release, `--smoke`, 5 runs) | 19.3 to 29.6 ms |
| Full repaint 1600×1000 (debug build, renderer optimised) | 2.9 ms |
| Release binaries | `qorral-gui` 1.5 MB, `qorral` 0.7 MB |

**Windows note:** release builds of `qorral-gui` use the GUI subsystem, so a shell does not wait for them and their stderr is not shown. For `--smoke` and `QORRAL_TRACE`, use `Start-Process -Wait -RedirectStandardError`.
