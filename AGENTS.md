# Hangar repository rules

Read `E:/Development/AGENTS.md` and `E:/Development/general-agent-rules.md` first. They win over anything here unless this file is stricter.

- **Product:** Hangar (working name), a VM manager GUI for Windows hosts in front of our own QEMU build (`E:/Development/winq-emu-qemu`, binaries in `E:/AgentData/winq-build/bin`). Status: proof of concept; no code until the owner has approved the plan (2026-11-15).
- **Pair Desk:** project `hangar`. Plan, handoff, backlog and evidence live there.
- **Identity:** commit only as `rennerdo30 <9086097+rennerdo30@users.noreply.github.com>`. Push with explicit refs only: `git push origin HEAD:refs/heads/<branch>`. No remote yet; visibility is the owner's call.
- **Hands off:** the renner.dev OS lab VMs (`E:/AgentData/renneros-lab/vms/*`), the QEMU fork and the virglrenderer fork belong to `claude-renner.dev-OS`. Ask that session; never modify them from here.
- **VM processes:** every QEMU the app or an agent starts records its PID and has a time limit; stop only recorded PIDs after checking the image is `qemu-system*`. Agent test VMs run headless unless the owner asks for a window.
- Paths: always `E:`; `D:` is the same drive. Branch limit: an active branch has a commit in the last 24 h; up to 10 is normal, 11 to 20 needs a recorded reason, 20 is the maximum.
