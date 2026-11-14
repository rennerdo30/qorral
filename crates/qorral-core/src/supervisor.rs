//! Creates, starts and stops VMs. Blocking calls for worker threads (and,
//! later, the daemon): disk creation, QEMU launch, QMP shutdown.
//!
//! Rules (AGENTS.md "VM processes"): every PID Qorral starts is recorded in
//! `run.json`; only a recorded PID that is still `qemu-system*` is ever ended.

use crate::host::QemuInstall;
use crate::library::{Library, RunRecord, VmEntry};
use crate::process::{is_our_qemu, kill_qemu, run_with_timeout};
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

/// Starts QEMU detached (it survives Qorral; the next start finds it again
/// through `run.json`), waits until QMP answers, and records the PID.
pub fn start(
    lib: &Library,
    q: &QemuInstall,
    entry: &VmEntry,
    display: Display,
    minutes: Option<u64>,
) -> Result<RunRecord, String> {
    if state(entry) == State::Running {
        return Err(format!("{} is already running", entry.config.name));
    }
    let ports = pick_ports(lib).ok_or("no free local ports for QMP")?;
    let paths = VmPaths::in_dir(&entry.dir);
    let fw = q.firmware_code();
    if entry.config.firmware == Firmware::Uefi && fw.is_none() {
        return Err("the QEMU build has no UEFI firmware (share/edk2-x86_64-code.fd)".into());
    }
    let launch = Launch {
        vm: &entry.config,
        paths: &paths,
        ports,
        display,
        accel: qemu::host_accel(),
        firmware_code: fw.as_deref(),
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
    detach(&mut cmd, qemu::effective_display(&launch) == Display::Headless);
    let mut child = cmd.spawn().map_err(|e| format!("cannot start QEMU: {e}"))?;
    let pid = child.id();
    let run = RunRecord {
        pid,
        qmp_port: ports.qmp,
        agent_port: ports.agent,
        started: now(),
        deadline: minutes.map_or(0, |m| now() + m * 60),
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

fn detach(cmd: &mut Command, headless: bool) {
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
    while Instant::now() < end {
        if !is_our_qemu(run.pid) {
            let _ = Library::save_run(entry, None);
            return Ok(State::Stopped);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if force && kill_qemu(run.pid) {
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
    let paths = VmPaths::in_dir(&entry.dir);
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
    };
    let exe = q.map_or("qemu-system-x86_64".to_string(), |q| q.system().display().to_string());
    let quote = |s: &str| if s.contains(' ') { format!("\"{s}\"") } else { s.to_string() };
    std::iter::once(quote(&exe)).chain(qemu::args(&l).iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ")
}
