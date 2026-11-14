# Qorral repository rules

Read `E:/Development/AGENTS.md` and `E:/Development/general-agent-rules.md` first. They win over anything here unless this file is stricter. `CLAUDE.md` imports this file; keep the two in step.

## Product (owner, 2026-11-15; verbatim words in Pair Desk HG-1)

- **What:** Qorral, our own desktop virtual machine manager in front of **our own QEMU build** (`E:/Development/winq-emu-qemu`, `E:/Development/winq-emu-virglrenderer`, binaries in `E:/AgentData/winq-build/bin`). GPU in the guest through virgl/Venus, WHPX on Windows, KVM on Linux, HVF on macOS.
- **Name:** Qorral (owner, 2026-11-15; name round HG-10). The local folder is still `E:/Development/hangar` and the Pair Desk slug is `hangar` (ids `HG-n`). **Public repository** `https://github.com/rennerdo30/qorral` (owner, 2026-11-15). No other products' names or trademarks in public files, UI, code comments, commits, PR texts or release notes; factual mentions of the OSes users install and of QEMU as the engine (with its licence notice) are allowed.
- **Audience:** power users first, but a normal person must manage without help. Simple by default, full depth one step away (every preset value editable, the QEMU command line visible).
- **Status:** proof of concept. No product code until the owner approves the plan on Pair Desk.
- **Platforms:** Windows, Linux and macOS now, others later. Each gets an executable, a package and an installer.

## Feature pillars (Pair Desk ids)

| Pillar | Issue | When |
|---|---|---|
| VM list, create, start, stop, status, console | HG-11 (PoC plan) | PoC |
| OS catalog: search, one-click presets for nearly anything installable, verified downloads, bring your own ISO | HG-9 | PoC (a few entries), then full |
| Unattended Windows install wizard (`autounattend`, drivers, progress phases) | HG-4 | after PoC |
| GPU acceleration for Linux and Windows guests (virgl, Venus) | HG-5 | after PoC; work in the forks belongs to `claude-renner.dev-OS` |
| **Sandbox mode:** a frozen base image, disposable overlays, deleted on close, locked down by policy (network, clipboard, folders, USB, lifetime) | HG-3 | M2; the owner needs it for a company project |
| Native host folder sharing, Windows host first | HG-6 | soon after PoC, approach agreed with `claude-renner.dev-OS` |
| Renderer and remote mode | HG-7 | from the first commit |
| Packages, installers, auto-update, beta channel, CI and release | HG-8 | from the first release |
| **Embedded guest display fast enough for games:** scanout to present < 10 ms, target < 5 ms, stretch 1 ms; the same for input (owner, 2026-11-15) | HG-14 | M1; zero-copy D3D11 → Vulkan, nothing queued |
| **Full CLI** with GUI parity (`--json`, exit codes, no prompts off a TTY), for integration into other apps | HG-12 | PoC, growing with every feature |
| **Background daemon** `qorrald`: VMs run without a window, GUI and CLI attach as clients | HG-13 | later run mode; the PoC already goes through a `Controller` API |

| **App VMs:** each application in its own (often disposable) VM; only its windows stream to the host desktop (rdp-rs or our own streamer) | HG-15 | later; keep the hooks: several display surfaces per VM, daemon-owned lifecycle, sandbox policy, a guest integration channel |

**Architecture invariant:** the GUI and the CLI never drive QEMU directly. Both call the same `qorral-core` API, in process now and over the daemon's IPC later. A GUI action without a CLI command is a parity defect.

## UX rule (owner, 2026-11-15)

> "I don't want you to make some cheap basic UI. I always want us to put usability first, user experience first. … We need to be creative. I want it to be an innovative modern UI. That's nice to use."

- Usability and UX first, from the first screen. Nobody should have to ask for "10x UX". The shared "information in one second" principle applies: state first (running, stopped, failed, what needs me), counts and states as badges and colour, live guest thumbnails instead of icons, nothing empty or decorative.
- Every UI change is checked in a `--snapshot` PNG against this rule before it goes to the owner.

## Performance rules (owner, 2026-11-15: "blazing fast … in millisecond area"; same model as ffwd and kwil)

A change that breaks one of these is a defect, even if its tests pass.

1. **Nothing blocks the UI thread.** QEMU starts, QMP, the guest agent, disk images, downloads and file I/O run on workers (`std::thread` + channels) and report back through the event loop.
2. **First frame first:** window and chrome before any VM work. Budget: ≤ 30 ms to first frame (hard 60 ms), input to pixels ≤ 1 frame, full repaint at 1600×1000 ≤ 4 ms. Measured, never assumed (`QORRAL_TRACE=1`, `--smoke`).
3. **Event-driven, damage-only drawing.** No frame loop; at idle 0 presents, 0 wakeups, 0 % CPU. Present only damaged rectangles; scrolling is a pixel copy.
4. **Two proper GUI renderers** (owner), on kwil's model (`E:/Development/kwil/docs/decisions/0001-renderer.md`, `crates/ui-gpu`):
   - The UI rasterises on the CPU into a damage-tracked backing store. `softbuffer` presents it: this is the **software renderer**, the first frame and the remote path.
   - A **Vulkan compositor** starts on a worker after the first frame. On macOS it uses Metal through the same layer. It presents the backing store and composites the guest display textures (on Windows, zero-copy from QEMU's shared D3D11 scanouts). It is never on the startup path, and a failed GPU falls back to the CPU for good.
5. **Remote-desktop mode** (owner): detected automatically (Windows `SM_REMOTESESSION`, switched live on `WM_WTSSESSION_CHANGE`; a setting elsewhere). It uses the software renderer, no animations, smooth scrolling or blinking, solid fills, damage-only presents, and guest thumbnails redrawn only when their pixels change. The embedded guest screen sends only its dirty rectangles.
6. **Never crash on user or guest data.** No `unwrap`/`expect` on I/O, QMP replies, guest output or settings; a panicking worker is reported and the window stays alive.
7. **Never lose user data.** Deleting a VM or disk asks first and names what is lost. Sandboxes are the only thing deleted on close, and only because that is their contract.
8. **Small dependency graph.** Every new crate earns its place (crate count, build time). No async runtime on the UI side. Heavy dependencies live in leaf crates.

## VM processes

- Every QEMU that Qorral or an agent starts runs in a Job Object or process group, records its PID and has a time limit for agent and test runs. Stop only recorded PIDs, after checking the image is `qemu-system*`.
- Agent test VMs run headless unless the owner asks for a window. Qorral's development VMs live in `E:/AgentData/qorral/`.
- Known QEMU quirks (from `lab.py`):
  - A guest-triggered reboot hangs under WHPX, so VMs run with `-no-reboot` and Qorral cold-boots them again.
  - WSL blocks local ports 44400–44601 while it runs, so check ports before a start.
  - The Venus SDL window is black for Windows guests until the fork's fix lands.
  - The QEMU build has no edk2 firmware yet.
- **Hands off:** the renner.dev OS lab VMs (`E:/AgentData/renneros-lab/vms/*`), the QEMU fork and the virglrenderer fork belong to `claude-renner.dev-OS`. Ask that session; never modify them from here.

## Build (ffwd setup; details in HG-2)

- `.cargo/config.example.toml` is committed; `.cargo/config.toml` is machine-local and gitignored. It sets:
  - shared sccache `E:/Development/_tools/sccache/0.19.0/…/sccache.exe` with `SCCACHE_DIR=E:/sccache`;
  - `rust-lld` as the Windows linker;
  - `target-dir = E:/Development/_cache/cargo-target/qorral`;
  - `jobs = 8`.
- **Nothing is built inside the repository.**
- Toolchain pinned in `rust-toolchain.toml`. Dev profile: line tables only, dependencies at opt-level 2, the renderer crate at opt-level 2.
- Scripts in `scripts/`: `run` (debug and release start), `check` (fmt, clippy `-D warnings`, tests, the same as CI), `snapshot` (renders the real UI offscreen to `artifacts/snapshot.png`), `package`. Each comes as `.bat` and `.sh`.
- **Releases:** a `v*` tag builds every platform into a DRAFT GitHub release with checksums; a `-beta.N` tag becomes a prerelease. The auto-updater has `stable` and `prerelease` channels (copied from `ffwd-update`). Only the owner publishes.

## Process

- **Pair Desk:** project `hangar` (ids `HG-n`). Plans, handoff, backlog and evidence live there.
- **Identity:** commit only as `rennerdo30 <9086097+rennerdo30@users.noreply.github.com>`. Push with explicit refs only: `git push origin HEAD:refs/heads/<branch>`. CI runs on GitHub-hosted runners (public repository rule).
- **Licence:** permissively licensed dependencies only (MIT, Apache-2.0, BSD, Zlib, ISC). QEMU is GPL and ships as a separate program that Qorral drives, never linked into it.
- Paths: always `E:`; `D:` is the same drive. Branch limit: an active branch has a commit in the last 24 h; up to 10 is normal, 11 to 20 needs a recorded reason, 20 is the maximum.
