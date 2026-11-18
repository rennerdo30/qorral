//! Creates, starts and stops VMs. Blocking calls for worker threads (and,
//! later, the daemon): disk creation, QEMU launch, QMP shutdown.
//!
//! Rules (AGENTS.md "VM processes"): every PID Qorral starts is recorded in
//! `run.json`; only a recorded PID that is still `qemu-system*` is ever ended.

use crate::host::QemuInstall;
use crate::library::{Library, RunRecord, VmEntry};
use crate::process::{is_our_qemu, kill_qemu, run_with_timeout, wait_gone};
use crate::qemu::{self, Display, Launch, Ports, VmPaths};
use crate::qmp::Qmp;
use crate::vm::{Firmware, VmConfig};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Loopback ports for VMs. Clear of 44400–44601, which WSL blocks while it runs.
const PORT_RANGE: std::ops::Range<u16> = 47100..47900;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Stopped,
    Running,
    /// `run.json` names a process that is gone (QEMU exited or crashed).
    Exited,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The VM's live state, from its run record and the process table.
pub fn state(entry: &VmEntry) -> State {
    match &entry.run {
        None => State::Stopped,
        Some(r) if is_our_qemu(r.pid) => State::Running,
        Some(_) => State::Exited,
    }
}

/// Brings an existing disk into the library without copying it: the new
/// machine's `vm.json` points at the disk (and UEFI store) where they are.
/// A missing UEFI store is created from the firmware template.
pub fn adopt(lib: &Library, q: &QemuInstall, config: &VmConfig) -> Result<VmEntry, String> {
    let disk = config.disk_path.as_deref().ok_or("adopt needs a disk image")?;
    if !std::path::Path::new(disk).is_file() {
        return Err(format!("no such disk image: {disk}"));
    }
    if let Some(e) = &config.efivars_path
        && !std::path::Path::new(e).is_file()
    {
        return Err(format!("no such UEFI variable store: {e}"));
    }
    let entry = lib.create(config).map_err(|e| format!("cannot create the machine folder: {e}"))?;
    if config.firmware == Firmware::Uefi && config.efivars_path.is_none() {
        let vars = q.firmware_vars().ok_or("the QEMU build has no UEFI variable template (share/edk2-*-vars.fd)")?;
        let mut c = Command::new(q.img());
        c.args(["convert", "-q", "-f", "raw", "-O", "qcow2"]).arg(vars).arg(entry.dir.join("efivars.qcow2"));
        check(run_with_timeout(c, Duration::from_secs(60)), "qemu-img convert")?;
    }
    Ok(entry)
}

/// Creates the VM folder, its disk and its UEFI variable store.
pub fn create(lib: &Library, q: &QemuInstall, config: &VmConfig) -> Result<VmEntry, String> {
    let entry = lib.create(config).map_err(|e| format!("cannot create the VM folder: {e}"))?;
    let paths = VmPaths::in_dir(&entry.dir);
    let mut c = Command::new(q.img());
    c.args(["create", "-q", "-f", "qcow2"]).arg(&paths.disk).arg(format!("{}G", config.disk_gib.max(1)));
    check(run_with_timeout(c, Duration::from_secs(60)), "qemu-img create")?;
    if config.firmware == Firmware::Uefi {
        let vars = q.firmware_vars().ok_or("the QEMU build has no UEFI variable template (share/edk2-*-vars.fd)")?;
        let mut c = Command::new(q.img());
        c.args(["convert", "-q", "-f", "raw", "-O", "qcow2"]).arg(vars).arg(&paths.efivars);
        check(run_with_timeout(c, Duration::from_secs(60)), "qemu-img convert")?;
    }
    Ok(entry)
}

fn check(r: Result<std::process::Output, String>, what: &str) -> Result<(), String> {
    let out = r?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The host address and port of a `hostfwd` rule (`tcp:127.0.0.1:2222-:22`,
/// `tcp::2222-:22`). `None` for rules this check does not understand.
pub fn forward_host(rule: &str) -> Option<(String, u16)> {
    let host = rule.split('-').next()?;
    let mut parts = host.split(':');
    let proto = parts.next()?;
    if proto != "tcp" {
        return None;
    }
    let addr = parts.next()?;
    let port: u16 = parts.next()?.parse().ok()?;
    let addr = if addr.is_empty() { "0.0.0.0" } else { addr };
    Some((addr.to_string(), port))
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Two free loopback ports that no other recorded VM uses.
pub fn pick_ports(lib: &Library) -> Option<Ports> {
    let taken: Vec<u16> =
        lib.list().iter().filter_map(|e| e.run.as_ref()).flat_map(|r| [r.qmp_port, r.agent_port]).collect();
    let mut free = PORT_RANGE.filter(|p| !taken.contains(p) && port_free(*p));
    Some(Ports { qmp: free.next()?, agent: free.next()? })
}

/// Options for one start; nothing here is saved to `vm.json`.
#[derive(Clone, Debug)]
pub struct StartOptions {
    pub display: Display,
    /// Hard time limit; a watchdog ends the VM when it passes.
    pub minutes: Option<u64>,
    /// Install medium for this start only (boots first).
    pub iso: Option<String>,
    /// Extra port forwards for this start (`tcp:127.0.0.1:2222-:22`).
    pub forwards: Vec<String>,
    /// Extra QEMU arguments for this start, after the saved ones.
    pub qemu_args: Vec<String>,
    /// Open QEMU's gdb stub on this loopback port.
    pub gdb: Option<u16>,
    /// Network for this start instead of the saved one.
    pub network: Option<crate::vm::NetworkMode>,
    /// Sound for this start instead of the saved setting.
    pub audio: Option<crate::vm::AudioMode>,
    /// Extra shared folders for this start.
    pub shares: Vec<crate::vm::Share>,
}

impl Default for StartOptions {
    fn default() -> Self {
        StartOptions {
            display: Display::Headless,
            minutes: None,
            iso: None,
            forwards: Vec::new(),
            qemu_args: Vec::new(),
            gdb: None,
            network: None,
            audio: None,
            shares: Vec::new(),
        }
    }
}

/// The settings QEMU actually gets: saved settings plus this start's options.
pub fn effective_config(saved: &VmConfig, o: &StartOptions) -> VmConfig {
    let mut c = saved.clone();
    if let Some(iso) = &o.iso {
        c.iso = Some(iso.clone());
    }
    if let Some(n) = o.network {
        c.network = n;
    }
    if let Some(a) = o.audio {
        c.audio = a;
    }
    c.port_forwards.extend(o.forwards.iter().cloned());
    c.shares.extend(o.shares.iter().cloned());
    c.extra_args.extend(o.qemu_args.iter().cloned());
    if let Some(port) = o.gdb {
        c.extra_args.push("-gdb".into());
        c.extra_args.push(format!("tcp:127.0.0.1:{port}"));
    }
    c
}

/// Starts QEMU detached (it survives Qorral; the next start finds it again
/// through `run.json`), waits until QMP answers, and records the PID.
pub fn start(lib: &Library, q: &QemuInstall, entry: &VmEntry, o: &StartOptions) -> Result<RunRecord, String> {
    let (display, minutes) = (o.display, o.minutes);
    let config = effective_config(&entry.config, o);
    if config.gpu.accelerated() && display == Display::Headless && !q.has_egl() {
        return Err(format!(
            "{} uses {} graphics; without a window that needs an EGL library next to QEMU \
             (libEGL.dll and libGLESv2.dll in {}), and this build has none. Start it with a window, \
             or use a QEMU build that ships EGL.",
            entry.config.name,
            config.gpu.label(),
            q.dir.display()
        ));
    }
    if state(entry) == State::Running {
        return Err(format!("{} is already running", entry.config.name));
    }
    // Ports can be briefly taken: by a VM that just stopped, or by WSL, which
    // reserves local ports while a distro runs and for about a minute after.
    // Qorral's range avoids WSL's usual 44400–44601, and still waits (bounded).
    let wait_end = Instant::now() + Duration::from_secs(180);
    // Host ports of forwards must be free, or QEMU fails with a terse message.
    for rule in &config.port_forwards {
        if let Some((addr, port)) = forward_host(rule)
            && std::net::TcpListener::bind((addr.as_str(), port)).is_err()
        {
            return Err(format!(
                "host port {port} for the forward `{rule}` is in use by another program; pick another port"
            ));
        }
    }
    let ports = loop {
        if let Some(p) = pick_ports(lib) {
            break p;
        }
        if Instant::now() >= wait_end {
            return Err("no free local ports for QMP after waiting 3 minutes".into());
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let paths = VmPaths::for_vm(&entry.dir, &config);
    write_shares(q, &config, &paths)?;
    if !paths.disk.is_file() {
        return Err(format!("the disk image is missing: {}", paths.disk.display()));
    }
    let fw = q.firmware_code();
    if config.firmware == Firmware::Uefi && fw.is_none() {
        return Err("the QEMU build has no UEFI firmware (share/edk2-x86_64-code.fd)".into());
    }
    let launch = Launch {
        vm: &config,
        paths: &paths,
        ports,
        display,
        accel: qemu::host_accel(),
        firmware_code: fw.as_deref(),
        refresh_mhz: crate::host::max_refresh_hz().map(|hz| hz * 1000),
    };
    let args = qemu::args(&launch);
    let log_path = entry.dir.join("qemu.log");
    let mut log = std::fs::File::create(&log_path).map_err(|e| format!("cannot write qemu.log: {e}"))?;
    {
        use std::io::Write;
        let _ = writeln!(log, "{} {}", q.system().display(), args.join(" "));
    }
    let err_log = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(q.system());
    cmd.args(&args).current_dir(&entry.dir).stdin(Stdio::null()).stdout(log).stderr(err_log);
    detach(&mut cmd, display == Display::Headless);
    let mut child = cmd.spawn().map_err(|e| format!("cannot start QEMU: {e}"))?;
    let pid = child.id();
    let run = RunRecord {
        pid,
        qmp_port: ports.qmp,
        agent_port: ports.agent,
        started: now(),
        deadline: minutes.map_or(0, |m| now() + m * 60),
        window: display == Display::Window,
        gdb_port: o.gdb,
    };
    Library::save_run(entry, Some(&run)).map_err(|e| format!("cannot record the VM's PID: {e}"))?;
    // Fail loudly on a bad start: wait for QMP, or for QEMU to exit.
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let _ = Library::save_run(entry, None);
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
            return Err(format!("QEMU exited at start ({status}): {last}"));
        }
        if Qmp::connect(ports.qmp, Duration::from_millis(500)).is_ok() {
            return Ok(run);
        }
        if Instant::now() >= end {
            return Err("QEMU started but QMP did not answer within 20 s".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Checks share names (unique, safe) and that every folder exists.
pub fn check_shares(shares: &[crate::vm::Share]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for s in shares {
        if !crate::vm::Share::valid_name(&s.name) {
            return Err(format!("share name `{}`: use letters, digits, - _ . (up to 64)", s.name));
        }
        if !seen.insert(s.name.to_ascii_lowercase()) {
            return Err(format!("two shares are called `{}`; give one another name", s.name));
        }
        if !std::path::Path::new(&s.path).is_dir() {
            return Err(format!("shared folder `{}` does not exist: {}", s.name, s.path));
        }
    }
    Ok(())
}

/// Writes the list QEMU's file server reads, or removes a stale one.
fn write_shares(q: &QemuInstall, config: &VmConfig, paths: &VmPaths) -> Result<(), String> {
    if config.shares.is_empty() {
        let _ = std::fs::remove_file(&paths.shares);
        return Ok(());
    }
    if !q.has_fs_server() {
        return Err(format!(
            "{} shares folders, but this QEMU build has no file server (qorral_fs) next to it in {}",
            config.name,
            q.dir.display()
        ));
    }
    check_shares(&config.shares)?;
    let list: Vec<serde_json::Value> = config
        .shares
        .iter()
        .map(|s| {
            let path = std::path::absolute(&s.path).map(|p| p.display().to_string()).unwrap_or_else(|_| s.path.clone());
            serde_json::json!({ "name": s.name, "path": path, "readonly": s.readonly })
        })
        .collect();
    // posix: Linux semantics (exact-case names, stored owner and mode); off
    // for Windows guests, which expect case-insensitive names.
    let posix = config.os != crate::vm::OsKind::Windows;
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "shares": list,
        "cache": config.share_cache.id(),
        "posix": posix,
    }))
    .unwrap_or_default();
    std::fs::write(&paths.shares, text).map_err(|e| format!("cannot write {}: {e}", paths.shares.display()))
}

fn detach(cmd: &mut Command, headless: bool) {
    crate::process::keep_std_handles_private();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Own process group: Ctrl+C in a terminal that ran `qorral start`
        // never reaches the VM. No console window either way.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = headless;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = headless;
        crate::process::no_console(cmd);
    }
}

/// Asks the guest to shut down (ACPI), or, with `force`, ends QEMU at once.
/// Returns once QEMU has exited or `timeout` passed (then it is still running).
pub fn stop(entry: &VmEntry, force: bool, timeout: Duration) -> Result<State, String> {
    let Some(run) = &entry.run else { return Ok(State::Stopped) };
    if !is_our_qemu(run.pid) {
        let _ = Library::save_run(entry, None);
        return Ok(State::Stopped);
    }
    let asked = Qmp::connect(run.qmp_port, Duration::from_secs(3))
        .and_then(|mut q| if force { q.quit() } else { q.powerdown() })
        .map_err(|e| e.to_string());
    let end = Instant::now() + timeout;
    // "Stopped" means the disk is free again: wait for the process to be
    // gone, not only for its exit code (a large VM takes a moment to unmap).
    let gone = || wait_gone(run.pid, Duration::from_secs(30));
    while Instant::now() < end {
        if !is_our_qemu(run.pid) {
            gone();
            let _ = Library::save_run(entry, None);
            return Ok(State::Stopped);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if force && kill_qemu(run.pid) {
        gone();
        let _ = Library::save_run(entry, None);
        return Ok(State::Stopped);
    }
    match asked {
        Ok(()) => Ok(State::Running),
        Err(e) => Err(format!("the VM did not stop: {e}")),
    }
}

/// Ends a VM whose time limit has passed (agent and test runs).
pub fn enforce_deadline(entry: &VmEntry) -> bool {
    match &entry.run {
        Some(r) if r.deadline != 0 && now() >= r.deadline && is_our_qemu(r.pid) => {
            stop(entry, true, Duration::from_secs(10)).is_ok()
        }
        _ => false,
    }
}

/// QEMU's command line for this VM, for the "show command" view and `qorral info`.
pub fn command_line(q: Option<&QemuInstall>, entry: &VmEntry) -> String {
    let paths = VmPaths::for_vm(&entry.dir, &entry.config);
    let fw = q.and_then(|q| q.firmware_code());
    let l = Launch {
        vm: &entry.config,
        paths: &paths,
        ports: entry
            .run
            .as_ref()
            .map_or(Ports { qmp: 0, agent: 0 }, |r| Ports { qmp: r.qmp_port, agent: r.agent_port }),
        display: Display::Headless,
        accel: qemu::host_accel(),
        firmware_code: fw.as_deref(),
        refresh_mhz: crate::host::max_refresh_hz().map(|hz| hz * 1000),
    };
    let exe = q.map_or("qemu-system-x86_64".to_string(), |q| q.system().display().to_string());
    let quote = |s: &str| if s.contains(' ') { format!("\"{s}\"") } else { s.to_string() };
    std::iter::once(quote(&exe)).chain(qemu::args(&l).iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_rules_name_their_host_port() {
        assert_eq!(forward_host("tcp:127.0.0.1:2222-:22"), Some(("127.0.0.1".into(), 2222)));
        assert_eq!(forward_host("tcp::13390-:3390"), Some(("0.0.0.0".into(), 13390)));
        assert_eq!(forward_host("udp:127.0.0.1:53-:53"), None);
        assert_eq!(forward_host("garbage"), None);
    }

    #[test]
    fn start_options_never_change_the_saved_settings() {
        let saved = VmConfig { port_forwards: vec!["tcp:127.0.0.1:1-:1".into()], ..VmConfig::default() };
        let o = StartOptions {
            iso: Some("/x.iso".into()),
            forwards: vec!["tcp:127.0.0.1:2-:2".into()],
            qemu_args: vec!["-rtc".into(), "base=localtime".into()],
            gdb: Some(1234),
            ..StartOptions::default()
        };
        let eff = effective_config(&saved, &o);
        assert_eq!(eff.port_forwards.len(), 2);
        assert_eq!(eff.iso.as_deref(), Some("/x.iso"));
        assert!(eff.extra_args.ends_with(&["-gdb".to_string(), "tcp:127.0.0.1:1234".to_string()]));
        assert_eq!(saved.port_forwards.len(), 1, "saved settings stay untouched");
        assert!(saved.iso.is_none());
    }
}
