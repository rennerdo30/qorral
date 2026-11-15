//! `qorral <command>`: everything the GUI does, for scripts and other apps.
//!
//! Contract (HG-12):
//! - `--json` prints one JSON document on stdout; its schema has `"schema": 1`.
//! - Exit codes: 0 done, 1 failed, 2 usage error, 3 no such machine, 4 timed out.
//! - Errors go to stderr. Nothing prompts; destructive commands need `--yes`.

use crate::model;
use qorral_core::guest::{AgentError, GuestAgent};
use qorral_core::host::QemuInstall;
use qorral_core::qemu::Display;
use qorral_core::qmp::Qmp;
use qorral_core::supervisor::{self, State};
use qorral_core::{GpuMode, Library, VmEntry};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "\
qorral: a fast virtual machine manager

usage: qorral [command] [options]      (no command: open the app)

  list                                  every machine and its state
  info <vm>                             settings, state and the QEMU command line
  create <name> [--iso FILE] [--preset ID] [--cpus N] [--memory SIZE] [--disk GB]
                [--gpu basic|opengl|vulkan|venus] [--bios] [--nvme]
                [--forward RULE]... [--qemu-arg ARG]...      (saved with the machine)
  adopt <name> --image DISK [--efivars FILE] [same options as create]
                                        use an existing disk where it is (no copy)
  start <vm> [--window] [--minutes N] [--iso FILE] [--forward RULE]... [--qemu-arg ARG]...
             [--gdb PORT]               headless unless --window; options apply to this start only
  stop <vm> [--force] [--timeout S]     ACPI shutdown, or end QEMU at once
  screenshot <vm> <out.png>             the guest screen as PNG
  keys <vm> <combo>...                  press keys, e.g. ctrl-alt-delete ret
  exec <vm> [--timeout S] [--wait-agent S] -- <program> [args]...
                                        run a program in the guest (QEMU guest agent; runs as
                                        SYSTEM/root); a non-zero guest exit code exits 1
  delete <vm> --yes                     delete a stopped machine and its disk
  host                                  the QEMU build and accelerator in use
  presets                               systems with tuned defaults

options: --json (machine-readable output), --home DIR (library location)
SIZE: 8 or 8G = 8 GB, 512M = 512 MB.  RULE: tcp:127.0.0.1:2222-:22
exit codes: 0 done, 1 failed, 2 usage error, 3 no such machine, 4 timed out
";

struct Opts {
    json: bool,
    rest: Vec<String>,
    flags: Vec<(String, Option<String>)>,
    /// Everything after a bare `--`, untouched (a guest command line).
    tail: Vec<String>,
}

/// Options that take a value.
const VALUED: &[&str] = &[
    "--iso",
    "--preset",
    "--cpus",
    "--memory",
    "--disk",
    "--gpu",
    "--minutes",
    "--timeout",
    "--home",
    "--name",
    "--forward",
    "--qemu-arg",
    "--gdb",
    "--image",
    "--efivars",
    "--wait-agent",
];

fn parse(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts { json: false, rest: Vec::new(), flags: Vec::new(), tail: Vec::new() };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--" {
            o.tail = it.collect();
            break;
        } else if a == "--json" {
            o.json = true;
        } else if VALUED.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
            o.flags.push((a, Some(v)));
        } else if a.starts_with("--") {
            o.flags.push((a, None));
        } else {
            o.rest.push(a);
        }
    }
    Ok(o)
}

impl Opts {
    fn flag(&self, n: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == n)
    }
    fn val(&self, n: &str) -> Option<&str> {
        self.flags.iter().find(|(k, _)| k == n).and_then(|(_, v)| v.as_deref())
    }
    /// Every value of a repeatable option, in order.
    fn vals(&self, n: &str) -> Vec<String> {
        self.flags.iter().filter(|(k, _)| k == n).filter_map(|(_, v)| v.clone()).collect()
    }
    fn num(&self, n: &str) -> Result<Option<u64>, String> {
        self.val(n).map(|v| v.parse::<u64>().map_err(|_| format!("{n} needs a number"))).transpose()
    }
}

enum Fail {
    Usage(String),
    NotFound(String),
    Failed(String),
    Timeout(String),
    /// A guest command ran and exited with this non-zero code.
    Guest(i64),
}

/// `8`, `8G`, `8GB` → 8192 MiB; `512M` → 512 MiB.
fn parse_memory(v: &str) -> Result<u32, String> {
    let s = v.trim().to_ascii_lowercase();
    let (num, mul) = if let Some(n) = s.strip_suffix("mb").or(s.strip_suffix('m')) {
        (n, 1)
    } else if let Some(n) = s.strip_suffix("gb").or(s.strip_suffix('g')) {
        (n, 1024)
    } else {
        (s.as_str(), 1024)
    };
    let n: u32 = num.trim().parse().map_err(|_| format!("--memory: `{v}` is not a size like 8G or 512M"))?;
    let mib = n.checked_mul(mul).ok_or("--memory is too large")?;
    if !(256..=1024 * 1024).contains(&mib) {
        return Err("--memory must be between 256M and 1024G".into());
    }
    Ok(mib)
}

/// Global options may come before the command (`qorral --home D list`):
/// they move behind it, where the parser reads every option.
fn hoist_globals(mut args: Vec<String>) -> Vec<String> {
    let mut globals = Vec::new();
    loop {
        match args.first().map(String::as_str) {
            Some("--json") => globals.push(args.remove(0)),
            Some("--home") if args.len() >= 2 => globals.extend(args.drain(..2)),
            _ => break,
        }
    }
    if args.is_empty() {
        return args;
    }
    // After the command, but before a `--` guest command line.
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args.splice(at..at, globals);
    args
}

pub fn run(args: Vec<String>) -> i32 {
    let args = hoist_globals(args);
    let Some(cmd) = args.first().cloned() else { return open_app() };
    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        print!("{USAGE}");
        return 0;
    }
    if cmd == "--version" || cmd == "version" {
        println!("qorral {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    let o = match parse(args[1..].to_vec()) {
        Ok(o) => o,
        Err(e) => return usage(&e),
    };
    let lib = Library::new(o.val("--home").map(|h| PathBuf::from(h).join("vms")).unwrap_or_else(Library::default_root));
    let result = match cmd.as_str() {
        "list" | "ls" => list(&lib, &o),
        "info" => info(&lib, &o),
        "create" => create(&lib, &o),
        "start" => start(&lib, &o),
        "adopt" => adopt(&lib, &o),
        "stop" => stop(&lib, &o),
        "screenshot" => screenshot(&lib, &o),
        "keys" => keys(&lib, &o),
        "exec" => exec(&lib, &o),
        "delete" | "rm" => delete(&lib, &o),
        "host" => host(&lib, &o),
        "presets" => presets(&o),
        "_watchdog" => watchdog(&lib, &o),
        other => Err(Fail::Usage(format!("unknown command `{other}`"))),
    };
    match result {
        Ok(()) => 0,
        Err(Fail::Usage(e)) => usage(&e),
        Err(Fail::NotFound(e)) => {
            report_error(o.json, "not_found", &e);
            3
        }
        Err(Fail::Failed(e)) => {
            report_error(o.json, "failed", &e);
            1
        }
        Err(Fail::Timeout(e)) => {
            report_error(o.json, "timeout", &e);
            4
        }
        Err(Fail::Guest(code)) => {
            if !o.json {
                eprintln!("qorral: the guest command exited with code {code}");
            }
            1
        }
    }
}

fn usage(e: &str) -> i32 {
    eprintln!("qorral: {e}\n\n{USAGE}");
    2
}

fn report_error(json: bool, kind: &str, msg: &str) {
    if json {
        println!("{}", json!({ "schema": 1, "ok": false, "error": { "kind": kind, "message": msg } }));
    }
    eprintln!("qorral: {msg}");
}

fn print_json(v: Value) {
    let mut v = v;
    v["schema"] = json!(1);
    v["ok"] = json!(true);
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// No command: start the app next to this executable and return at once.
fn open_app() -> i32 {
    let gui = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(if cfg!(windows) { "qorral-gui.exe" } else { "qorral-gui" })));
    match gui {
        Some(g) if g.is_file() => match std::process::Command::new(&g).spawn() {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("qorral: cannot open the app: {e}");
                1
            }
        },
        _ => {
            print!("{USAGE}");
            0
        }
    }
}

fn find(lib: &Library, o: &Opts) -> Result<VmEntry, Fail> {
    let want = o.rest.first().ok_or_else(|| Fail::Usage("which machine?".into()))?;
    let all = lib.list();
    all.iter()
        .find(|e| &e.id == want)
        .or_else(|| all.iter().find(|e| e.config.name.eq_ignore_ascii_case(want)))
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("no machine `{want}` in {}", lib.root.display())))
}

fn qemu() -> Result<QemuInstall, Fail> {
    QemuInstall::find().ok_or_else(|| {
        Fail::Failed("QEMU not found: set QORRAL_QEMU to the folder with qemu-system-x86_64, or put it on PATH".into())
    })
}

fn state_name(e: &VmEntry) -> &'static str {
    match supervisor::state(e) {
        State::Running => "running",
        State::Stopped | State::Exited => "off",
    }
}

fn entry_json(e: &VmEntry) -> Value {
    let c = &e.config;
    json!({
        "id": e.id,
        "name": c.name,
        "state": state_name(e),
        "system": model::os_label(c),
        "cpus": c.cpus,
        "memory_mib": c.memory_mib,
        "disk_gib": c.disk_gib,
        "gpu": c.gpu,
        "firmware": c.firmware,
        "disk_bus": c.disk_bus,
        "iso": c.iso,
        "pid": e.run.as_ref().filter(|_| supervisor::state(e) == State::Running).map(|r| r.pid),
        "qmp_port": e.run.as_ref().filter(|_| supervisor::state(e) == State::Running).map(|r| r.qmp_port),
        "dir": e.dir,
        "error": e.error,
    })
}

fn list(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let all = lib.list();
    if o.json {
        print_json(json!({ "machines": all.iter().map(entry_json).collect::<Vec<_>>() }));
        return Ok(());
    }
    if all.is_empty() {
        println!("No machines yet. Create one:  qorral create my-vm --iso path/to/image.iso");
        return Ok(());
    }
    let w = all.iter().map(|e| e.id.len()).max().unwrap_or(2).max(2);
    println!("{:<w$}  {:<8}  {:<18}  {:>4}  {:>7}  GPU", "ID", "STATE", "SYSTEM", "CPUS", "MEMORY");
    for e in &all {
        let c = &e.config;
        println!(
            "{:<w$}  {:<8}  {:<18}  {:>4}  {:>5} GB  {}",
            e.id,
            if e.error.is_some() { "broken" } else { state_name(e) },
            model::os_label(c),
            c.cpus,
            c.memory_mib / 1024,
            c.gpu.label()
        );
    }
    Ok(())
}

fn info(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let q = QemuInstall::find();
    let cmd = supervisor::command_line(q.as_ref(), &e);
    if o.json {
        let mut v = entry_json(&e);
        v["command"] = json!(cmd);
        print_json(json!({ "machine": v }));
    } else {
        println!("{} ({})", e.config.name, e.id);
        println!("  state     {}", state_name(&e));
        println!("  system    {}", model::os_label(&e.config));
        println!("  folder    {}", e.dir.display());
        if let Some(err) = &e.error {
            println!("  error     {err}");
        }
        println!("  command   {cmd}");
    }
    Ok(())
}

fn create(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let iso = o.val("--iso").map(PathBuf::from);
    if let Some(i) = &iso
        && !i.is_file()
    {
        return Err(Fail::Failed(format!("no such image: {}", i.display())));
    }
    let preset = o.val("--preset");
    if let Some(p) = preset
        && model::preset(p).is_none()
    {
        return Err(Fail::Usage(format!("unknown preset `{p}` (see `qorral presets`)")));
    }
    let name = o.rest.first().cloned().unwrap_or_else(|| model::default_name(preset, iso.as_deref()));
    let mut c = model::config_for(&name, preset, iso.as_deref());
    if let Some(n) = o.num("--cpus").map_err(Fail::Usage)? {
        c.cpus = n.clamp(1, 256) as u32;
    }
    apply_hardware(&mut c, o)?;
    let q = qemu()?;
    let e = supervisor::create(lib, &q, &c).map_err(Fail::Failed)?;
    if o.json {
        print_json(json!({ "machine": entry_json(&e) }));
    } else {
        println!("Created {} ({}) in {}", e.config.name, e.id, e.dir.display());
        println!("Start it:  qorral start {} --window", e.id);
    }
    Ok(())
}

/// Hardware options shared by `create` and `adopt` (saved with the machine).
fn apply_hardware(c: &mut qorral_core::VmConfig, o: &Opts) -> Result<(), Fail> {
    if let Some(n) = o.num("--cpus").map_err(Fail::Usage)? {
        c.cpus = n.clamp(1, 256) as u32;
    }
    if let Some(m) = o.val("--memory") {
        c.memory_mib = parse_memory(m).map_err(Fail::Usage)?;
    }
    if let Some(n) = o.num("--disk").map_err(Fail::Usage)? {
        c.disk_gib = n.clamp(1, 65536) as u32;
    }
    if let Some(g) = o.val("--gpu") {
        c.gpu = match g {
            "basic" => GpuMode::Basic,
            "opengl" | "gl" => GpuMode::Opengl,
            "vulkan" | "venus" => GpuMode::Vulkan,
            _ => return Err(Fail::Usage("--gpu is basic, opengl (gl) or vulkan (venus)".into())),
        };
    }
    if o.flag("--bios") {
        c.firmware = qorral_core::Firmware::Bios;
    }
    if o.flag("--nvme") {
        c.disk_bus = qorral_core::DiskBus::Nvme;
    }
    c.port_forwards.extend(o.vals("--forward"));
    c.extra_args.extend(o.vals("--qemu-arg"));
    Ok(())
}

fn adopt(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let image = o.val("--image").ok_or_else(|| Fail::Usage("adopt needs --image DISK".into()))?;
    let image = std::path::absolute(image).map_err(|e| Fail::Failed(e.to_string()))?;
    let name = o.rest.first().cloned().ok_or_else(|| Fail::Usage("adopt needs a name".into()))?;
    let preset = o.val("--preset");
    let mut c = model::config_for(&name, preset, None);
    c.disk_path = Some(image.display().to_string());
    if let Some(v) = o.val("--efivars") {
        let v = std::path::absolute(v).map_err(|e| Fail::Failed(e.to_string()))?;
        c.efivars_path = Some(v.display().to_string());
    }
    apply_hardware(&mut c, o)?;
    let q = qemu()?;
    let e = supervisor::adopt(lib, &q, &c).map_err(Fail::Failed)?;
    if o.json {
        print_json(json!({ "machine": entry_json(&e) }));
    } else {
        println!("Adopted {} ({}): disk stays at {}", e.config.name, e.id, image.display());
    }
    Ok(())
}

fn start(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let q = qemu()?;
    let minutes = o.num("--minutes").map_err(Fail::Usage)?;
    let display = if o.flag("--window") { Display::Window } else { Display::Headless };
    let iso = match o.val("--iso") {
        Some(i) => {
            let p = std::path::absolute(i).map_err(|err| Fail::Failed(err.to_string()))?;
            if !p.is_file() {
                return Err(Fail::Failed(format!("no such image: {}", p.display())));
            }
            Some(p.display().to_string())
        }
        None => None,
    };
    let gdb = o.num("--gdb").map_err(Fail::Usage)?.map(|p| p.clamp(1, 65535) as u16);
    let opts = supervisor::StartOptions {
        display,
        minutes,
        iso,
        forwards: o.vals("--forward"),
        qemu_args: o.vals("--qemu-arg"),
        gdb,
    };
    let run = supervisor::start(lib, &q, &e, &opts).map_err(Fail::Failed)?;
    if minutes.is_some() {
        spawn_watchdog(&e.id, o.val("--home"));
    }
    if o.json {
        print_json(json!({ "machine": entry_json(&Library::load_dir(&e.dir)), "pid": run.pid }));
    } else {
        println!("Started {} (pid {}, QMP 127.0.0.1:{})", e.config.name, run.pid, run.qmp_port);
    }
    Ok(())
}

/// A detached `qorral _watchdog <vm>` ends the VM at its time limit, even
/// after this command has exited.
fn spawn_watchdog(id: &str, home: Option<&str>) {
    let Ok(me) = std::env::current_exe() else { return };
    let mut c = std::process::Command::new(me);
    c.args(["_watchdog", id]);
    if let Some(h) = home {
        c.args(["--home", h]);
    }
    qorral_core::process::no_console(&mut c);
    let _ = c
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn watchdog(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let Some(pid) = e.run.as_ref().map(|r| r.pid) else { return Ok(()) };
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let now = Library::load_dir(&e.dir);
        // Done when the VM stopped or another run replaced this one.
        if now.run.as_ref().map(|r| r.pid) != Some(pid) || supervisor::state(&now) != State::Running {
            return Ok(());
        }
        if supervisor::enforce_deadline(&now) {
            return Ok(());
        }
    }
}

fn stop(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let force = o.flag("--force");
    let timeout = Duration::from_secs(o.num("--timeout").map_err(Fail::Usage)?.unwrap_or(if force { 10 } else { 90 }));
    let st = supervisor::stop(&e, force, timeout).map_err(Fail::Failed)?;
    if st == State::Running {
        return Err(Fail::Timeout(format!(
            "{} is still running after {} s (the guest ignored the shutdown request; use --force)",
            e.config.name,
            timeout.as_secs()
        )));
    }
    if o.json {
        print_json(json!({ "machine": entry_json(&Library::load_dir(&e.dir)) }));
    } else {
        println!("Stopped {}", e.config.name);
    }
    Ok(())
}

fn connect(e: &VmEntry) -> Result<Qmp, Fail> {
    let run = e.run.as_ref().filter(|_| supervisor::state(e) == State::Running);
    let run = run.ok_or_else(|| Fail::Failed(format!("{} is not running", e.config.name)))?;
    Qmp::connect(run.qmp_port, Duration::from_secs(5)).map_err(|err| Fail::Failed(err.to_string()))
}

fn screenshot(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let out = o.rest.get(1).ok_or_else(|| Fail::Usage("screenshot needs an output file".into()))?;
    let out = std::path::absolute(Path::new(out)).map_err(|err| Fail::Failed(err.to_string()))?;
    // QEMU writes PPM; Qorral converts it, so any QEMU build works.
    let tmp = e.dir.join("screenshot.ppm");
    connect(&e)?.screendump_ppm(&tmp).map_err(|err| Fail::Failed(err.to_string()))?;
    let img = std::fs::read(&tmp).ok().and_then(|d| qorral_core::image::parse_ppm(&d));
    let _ = std::fs::remove_file(&tmp);
    let img = img.ok_or_else(|| Fail::Failed("QEMU wrote an unreadable screenshot".into()))?;
    let rgba: Vec<u8> = img.px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect();
    crate::snapshot::write_png(&out, &rgba, img.w, img.h).map_err(Fail::Failed)?;
    if o.json {
        print_json(json!({ "file": out }));
    } else {
        println!("{}", out.display());
    }
    Ok(())
}

fn keys(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    if o.rest.len() < 2 {
        return Err(Fail::Usage("keys needs at least one key, e.g. ctrl-alt-delete".into()));
    }
    let mut q = connect(&e)?;
    for combo in &o.rest[1..] {
        let parts: Vec<&str> = combo.split(['-', '+']).collect();
        q.send_keys(&parts).map_err(|err| Fail::Failed(err.to_string()))?;
    }
    if o.json {
        print_json(json!({}));
    }
    Ok(())
}

fn exec(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    if o.tail.is_empty() {
        return Err(Fail::Usage("exec needs a command after `--`, e.g. qorral exec win -- cmd /c ver".into()));
    }
    let run = e.run.as_ref().filter(|_| supervisor::state(&e) == State::Running);
    let run = run.ok_or_else(|| Fail::Failed(format!("{} is not running", e.config.name)))?;
    let timeout = Duration::from_secs(o.num("--timeout").map_err(Fail::Usage)?.unwrap_or(60));
    let wait = Duration::from_secs(o.num("--wait-agent").map_err(Fail::Usage)?.unwrap_or(0));
    let failed = |err: AgentError| match err {
        AgentError::Timeout => {
            Fail::Timeout(format!("the guest command did not finish within {} s", timeout.as_secs()))
        }
        other => Fail::Failed(other.to_string()),
    };
    let mut agent = GuestAgent::wait_connect(run.agent_port, wait).map_err(failed)?;
    let out = agent.exec(&o.tail, timeout).map_err(failed)?;
    if o.json {
        let mut v = json!({ "exit_code": out.exit_code, "stdout": out.stdout, "stderr": out.stderr });
        v["schema"] = json!(1);
        v["ok"] = json!(out.exit_code == 0);
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    } else {
        use std::io::Write;
        let _ = std::io::stdout().write_all(out.stdout.as_bytes());
        let _ = std::io::stderr().write_all(out.stderr.as_bytes());
    }
    if out.exit_code == 0 { Ok(()) } else { Err(Fail::Guest(out.exit_code)) }
}

fn delete(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    if supervisor::state(&e) == State::Running {
        return Err(Fail::Failed(format!("{} is running; stop it first", e.config.name)));
    }
    if !o.flag("--yes") {
        return Err(Fail::Usage(format!(
            "this deletes {} and its disk in {}; add --yes to confirm",
            e.config.name,
            e.dir.display()
        )));
    }
    // Only a folder inside the library that holds a vm.json is ever removed.
    let inside = e.dir.parent().is_some_and(|p| p == lib.root) && e.dir.join("vm.json").is_file();
    if !inside {
        return Err(Fail::Failed(format!("refusing to delete {}: not a machine folder", e.dir.display())));
    }
    std::fs::remove_dir_all(&e.dir).map_err(|err| Fail::Failed(format!("cannot delete {}: {err}", e.dir.display())))?;
    if o.json {
        print_json(json!({ "deleted": e.id }));
    } else {
        println!("Deleted {}", e.config.name);
    }
    Ok(())
}

fn host(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let q = QemuInstall::find();
    let v = json!({
        "accel": qorral_core::qemu::host_accel(),
        "qemu": q.as_ref().map(|q| json!({
            "dir": q.dir,
            "version": q.version(),
            "displays": q.displays(),
            "uefi": q.firmware_code().is_some(),
        })),
        "library": lib.root,
    });
    if o.json {
        print_json(v);
    } else {
        println!("accelerator  {}", qorral_core::qemu::host_accel());
        match &q {
            Some(q) => {
                println!(
                    "qemu         {} ({})",
                    q.version().unwrap_or_else(|| "unknown version".into()),
                    q.dir.display()
                );
                println!("displays     {}", q.displays().join(", "));
                println!("uefi         {}", if q.firmware_code().is_some() { "yes" } else { "no firmware found" });
            }
            None => println!("qemu         not found (set QORRAL_QEMU)"),
        }
        println!("library      {}", lib.root.display());
    }
    Ok(())
}

fn presets(o: &Opts) -> Result<(), Fail> {
    if o.json {
        let v: Vec<Value> = model::PRESETS
            .iter()
            .map(|p| json!({ "id": p.id, "name": p.label, "cpus": p.cpus, "memory_mib": p.memory_mib, "disk_gib": p.disk_gib }))
            .collect();
        print_json(json!({ "presets": v }));
    } else {
        for p in &model::PRESETS {
            println!(
                "{:<12} {:<12} {} vCPU, {} GB memory, {} GB disk",
                p.id,
                p.label,
                p.cpus,
                p.memory_mib / 1024,
                p.disk_gib
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sizes_parse_like_people_write_them() {
        assert_eq!(parse_memory("8").unwrap(), 8192);
        assert_eq!(parse_memory("8G").unwrap(), 8192);
        assert_eq!(parse_memory("6gb").unwrap(), 6144);
        assert_eq!(parse_memory("512M").unwrap(), 512);
        assert!(parse_memory("100M").is_err(), "below the floor");
        assert!(parse_memory("lots").is_err());
    }

    #[test]
    fn repeatable_options_keep_every_value_in_order() {
        let o =
            parse(["vm", "--forward", "a", "--qemu-arg", "-rtc", "--forward", "b"].map(String::from).to_vec()).unwrap();
        assert_eq!(o.vals("--forward"), ["a", "b"]);
        assert_eq!(o.vals("--qemu-arg"), ["-rtc"]);
        assert_eq!(o.rest, ["vm"]);
    }

    #[test]
    fn global_options_may_come_before_the_command() {
        let a = hoist_globals(["--home", "/h", "--json", "exec", "win", "--", "cmd"].map(String::from).to_vec());
        assert_eq!(a, ["exec", "win", "--home", "/h", "--json", "--", "cmd"]);
        assert_eq!(hoist_globals(["list"].map(String::from).to_vec()), ["list"]);
        assert!(hoist_globals(["--json"].map(String::from).to_vec()).is_empty());
    }

    #[test]
    fn everything_after_double_dash_is_the_guest_command() {
        let o =
            parse(["win", "--timeout", "5", "--", "cmd", "/c", "--json", "ver"].map(String::from).to_vec()).unwrap();
        assert_eq!(o.rest, ["win"]);
        assert_eq!(o.val("--timeout"), Some("5"));
        assert_eq!(o.tail, ["cmd", "/c", "--json", "ver"]);
        assert!(!o.json, "--json after -- belongs to the guest command");
    }
}
