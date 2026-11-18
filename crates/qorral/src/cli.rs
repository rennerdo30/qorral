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
use qorral_core::sandbox::{self, Finished};
use qorral_core::supervisor::{self, State};
use qorral_core::{GpuMode, Library, NetworkMode, VmEntry};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const USAGE: &str = "\
qorral: a fast virtual machine manager

usage: qorral [command] [options]      (no command: open the app)

  list                                  every machine and its state
  info <vm>                             settings, state and the QEMU command line
  create <name> [--iso FILE] [--preset ID] [--cpus N] [--memory SIZE] [--disk GB]
                [--gpu basic|opengl|vulkan|venus] [--bios] [--nvme] [--network MODE]
                [--audio off|on|mic] [--forward RULE]... [--qemu-arg ARG]...
                                        (saved with the machine)
  adopt <name> --image DISK [--efivars FILE] [same options as create]
                                        use an existing disk where it is (no copy)
  start <vm> [--window] [--minutes N] [--iso FILE] [--network MODE] [--forward RULE]...
             [--share PATH[:ro]]... [--audio off|on|mic] [--qemu-arg ARG]... [--gdb PORT]
                                        headless unless --window; options apply to this start only
  stop <vm> [--force] [--timeout S]     ACPI shutdown, or end QEMU at once
  screenshot <vm> <out.png>             the guest screen as PNG
  keys <vm> <combo>...                  press keys, e.g. ctrl-alt-delete ret
  latency <vm> [--samples N] [--key QCODE] [--interval MS]
                                        input-to-display latency: presses a key and times until
                                        the guest's screen changes (run `qorral-bench --scene
                                        latency` in the guest first; it flashes white on a key)
  exec <vm> [--timeout S] [--wait-agent S] -- <program> [args]...
                                        run a program in the guest (QEMU guest agent; runs as
                                        SYSTEM/root); a non-zero guest exit code exits 1.
                                        Git Bash rewrites /paths after --: set MSYS_NO_PATHCONV=1
  delete <vm> --yes                     delete a stopped machine and its disk
  share add <vm> <folder> [--name N] [--readonly]
                                        share a host folder (guest: mount -t virtiofs qorral DIR)
  share remove <vm> <name> | share list <vm>
  share cache <vm> auto|strict          auto (default): fast with small files, host changes
                                        show up within a second; strict: at once, slower
                                        guest writes reach the host when the file is closed
                                        (or the program ends, e.g. when exec returns); for a
                                        file still open in the guest, run sync or fsync first
  base freeze <vm> [--name N]           freeze a stopped machine into a read-only base
  base list                             bases and what uses them
  base delete <name> --yes              delete a base nothing uses any more
  sandbox run <base> [--profile FILE] [--network MODE] [--forward RULE]... [--minutes N]
              [--window] [--cpus N] [--memory SIZE] [--share PATH[:rw]]... [--qemu-arg ARG]...
              [-- startup command]           shares are read-only unless :rw
                                        a disposable machine on a base; deleted when it ends
  sandbox list                          running sandboxes (cleans up leftovers first)
  sandbox stop <id> [--force]           end a sandbox (it is deleted)
  sandbox keep <id>                     keep a sandbox as a normal machine when it ends
  host                                  the QEMU build and accelerator in use
  presets                               systems with tuned defaults

options: --json (machine-readable output), --home DIR (library location)
SIZE: 8 or 8G = 8 GB, 512M = 512 MB.  RULE: tcp:127.0.0.1:2222-:22
MODE: none (no network; sandbox default), isolated (forwards only), internet (host blocked),
      nat (machine default)
Sandbox ids work with every machine command (info, exec, screenshot, keys, stop).
exit codes: 0 done, 1 failed, 2 usage error, 3 no such machine, 4 timed out
";

#[derive(Debug)]
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
    "--network",
    "--audio",
    "--profile",
    "--share",
    "--samples",
    "--key",
    "--interval",
];

/// Options without a value.
const SWITCHES: &[&str] = &["--bios", "--force", "--nvme", "--readonly", "--window", "--yes"];

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
            // `--name=value` is the same as `--name value`; anything unknown is a usage error,
            // never silently ignored.
            match a.split_once('=') {
                Some((name, v)) if VALUED.contains(&name) => o.flags.push((name.to_string(), Some(v.to_string()))),
                Some((name, _)) if SWITCHES.contains(&name) => return Err(format!("{name} takes no value")),
                None if SWITCHES.contains(&a.as_str()) => o.flags.push((a, None)),
                _ => return Err(format!("unknown option `{a}` (see `qorral help`)")),
            }
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
        "latency" => latency(&lib, &o),
        "exec" => exec(&lib, &o),
        "delete" | "rm" => delete(&lib, &o),
        "host" => host(&lib, &o),
        "presets" => presets(&o),
        "_watchdog" => watchdog(&lib, &o),
        "base" => base_cmd(&lib, &o),
        "share" | "shares" => share_cmd(&lib, &o),
        "sandbox" | "sbx" => sandbox_cmd(&lib, &o),
        "_reap" => reap_cmd(&lib, &o),
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
    find_named(lib, want)
}

/// A machine by id or name; sandboxes count too.
fn find_named(lib: &Library, want: &str) -> Result<VmEntry, Fail> {
    let mut all = lib.list();
    all.extend(sandbox::sandboxes(lib).list());
    all.iter()
        .find(|e| e.id == want)
        .or_else(|| all.iter().find(|e| e.config.name.eq_ignore_ascii_case(want)))
        .cloned()
        .ok_or_else(|| Fail::NotFound(format!("no machine or sandbox `{want}` in {}", lib.root.display())))
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
        "network": c.network.id(),
        "audio": c.audio.id(),
        "shares": c.shares,
        "base": c.base,
        "sandbox": sandbox::marker(e).map(|m| json!({ "base": m.base, "keep": m.keep, "created": m.created })),
        "pid": e.run.as_ref().filter(|_| supervisor::state(e) == State::Running).map(|r| r.pid),
        "qmp_port": e.run.as_ref().filter(|_| supervisor::state(e) == State::Running).map(|r| r.qmp_port),
        "dir": e.dir,
        "error": e.error,
    })
}

fn list(lib: &Library, o: &Opts) -> Result<(), Fail> {
    sandbox::sweep(lib);
    let mut all = lib.list();
    all.extend(sandbox::sandboxes(lib).list());
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
    if let Some(n) = network_opt(o)? {
        c.network = n;
    }
    if let Some(a) = audio_opt(o)? {
        c.audio = a;
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
        network: network_opt(o)?,
        audio: audio_opt(o)?,
        shares: o.vals("--share").iter().map(|v| qorral_core::Share::parse(v, false)).collect(),
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
    spawn_helper("_watchdog", id, home);
}

/// Starts a detached `qorral <cmd> <id>` that outlives this command.
fn spawn_helper(cmd: &str, id: &str, home: Option<&str>) {
    let Ok(me) = std::env::current_exe() else { return };
    let mut c = std::process::Command::new(me);
    c.args([cmd, id]);
    if let Some(h) = home {
        c.args(["--home", h]);
    }
    qorral_core::process::no_console(&mut c);
    qorral_core::process::keep_std_handles_private();
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

fn latency(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let number = |name: &str, default: u64, min: u64, max: u64| -> Result<u64, Fail> {
        match o.val(name) {
            None => Ok(default),
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|n| (min..=max).contains(n))
                .ok_or_else(|| Fail::Usage(format!("{name} must be a number from {min} to {max}"))),
        }
    };
    let samples = number("--samples", 20, 1, 1000)?;
    // The benchmark's latency scene stays white for 250 ms after a key; the next probe must see it dark.
    let interval = number("--interval", 500, 300, 60_000)?;
    let key = o.val("--key").unwrap_or("spc");
    let mut q = connect(&e)?;
    let mut ms: Vec<f64> = Vec::new();
    // Per stage (see LatencyStages::spans), in ms, from samples where every mark was seen.
    let mut spans: [Vec<f64>; 4] = Default::default();
    let mut timeouts = 0u64;
    for i in 0..samples {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(interval));
        }
        let s = q.latency_probe(key, Duration::from_secs(2)).map_err(|err| Fail::Failed(err.to_string()))?;
        if s.timed_out {
            timeouts += 1;
            continue;
        }
        ms.push(s.elapsed_us as f64 / 1000.0);
        if let Some(sp) = s.stages.and_then(|st| st.spans(s.elapsed_us)) {
            for (list, us) in spans.iter_mut().zip(sp) {
                list.push(us as f64 / 1000.0);
            }
        }
    }
    const STAGES: [(&str, &str); 4] = [
        ("input_ms", "key to guest read"),
        ("guest_ms", "guest to present"),
        ("host_gpu_ms", "present to display update"),
        ("display_ms", "display listeners"),
    ];
    let median = |v: &[f64]| {
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        s.get(s.len().saturating_sub(1) / 2).copied()
    };
    let stage_medians: Vec<Option<f64>> = spans.iter().map(|v| median(v)).collect();
    if ms.is_empty() {
        return Err(Fail::Failed(format!(
            "the screen did not change after any of {samples} key presses: run `qorral-bench --scene latency` in \
             the guest, in the foreground"
        )));
    }
    let mut sorted = ms.clone();
    sorted.sort_by(f64::total_cmp);
    let at = |q: f64| sorted[((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1];
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    if o.json {
        let mut stages = serde_json::Map::new();
        for ((key, _), (m, values)) in STAGES.iter().zip(stage_medians.iter().zip(&spans)) {
            stages.insert(key.to_string(), json!({ "median": m, "values": values }));
        }
        print_json(json!({
            "samples": ms.len(), "timeouts": timeouts, "key": key,
            "median_ms": at(0.5), "p90_ms": at(0.9), "min_ms": sorted[0], "max_ms": sorted[sorted.len() - 1],
            "mean_ms": mean, "values_ms": ms, "stages": stages, "staged_samples": spans[0].len(),
        }));
    } else {
        println!(
            "input to display (key into QEMU to the changed frame at QEMU's display): median {:.2} ms, \
             p90 {:.2}, min {:.2}, max {:.2}, mean {:.2} ({} samples{})",
            at(0.5),
            at(0.9),
            sorted[0],
            sorted[sorted.len() - 1],
            mean,
            ms.len(),
            if timeouts > 0 { format!(", {timeouts} timed out") } else { String::new() }
        );
        if !spans[0].is_empty() {
            let parts: Vec<String> = STAGES
                .iter()
                .zip(&stage_medians)
                .filter_map(|((_, label), m)| m.map(|m| format!("{label} {m:.2}")))
                .collect();
            println!("  stage medians, ms ({} samples): {}", spans[0].len(), parts.join(", "));
        }
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

fn audio_opt(o: &Opts) -> Result<Option<qorral_core::AudioMode>, Fail> {
    o.val("--audio")
        .map(|v| {
            qorral_core::AudioMode::parse(v).ok_or_else(|| Fail::Usage(format!("--audio is off, on or mic, not `{v}`")))
        })
        .transpose()
}

fn network_opt(o: &Opts) -> Result<Option<NetworkMode>, Fail> {
    o.val("--network")
        .map(|v| {
            NetworkMode::parse(v)
                .ok_or_else(|| Fail::Usage(format!("--network is none, isolated, internet or nat, not `{v}`")))
        })
        .transpose()
}

fn share_cmd(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let sub = o.rest.first().map(String::as_str).unwrap_or("list");
    let vm_name = o.rest.get(1).ok_or_else(|| Fail::Usage(format!("share {sub} needs a machine")))?;
    let e = find_named(lib, vm_name)?;
    if sandbox::marker(&e).is_some() {
        return Err(Fail::Usage("sandbox shares are set when it starts (sandbox run --share)".into()));
    }
    let mut c = e.config.clone();
    match sub {
        "list" | "ls" => {
            if o.json {
                print_json(json!({
                    "machine": e.id,
                    "tag": qorral_core::qemu::SHARE_TAG,
                    "cache": c.share_cache.id(),
                    "shares": c.shares,
                }));
            } else if c.shares.is_empty() {
                println!("{} shares no folders. Add one:  qorral share add {} <folder>", e.config.name, e.id);
            } else {
                for s in &c.shares {
                    println!("{:<20} {:<3} {}", s.name, if s.readonly { "ro" } else { "rw" }, s.path);
                }
            }
            return Ok(());
        }
        "add" => {
            let folder = o.rest.get(2).ok_or_else(|| Fail::Usage("share add needs a folder".into()))?;
            let path = std::path::absolute(folder).map_err(|err| Fail::Failed(err.to_string()))?;
            let mut share = qorral_core::Share::parse(&path.display().to_string(), o.flag("--readonly"));
            if let Some(n) = o.val("--name") {
                share.name = n.to_string();
            }
            c.shares.retain(|s| !s.name.eq_ignore_ascii_case(&share.name));
            c.shares.push(share.clone());
            supervisor::check_shares(&c.shares).map_err(Fail::Failed)?;
            Library::save_config(&e, &c).map_err(|err| Fail::Failed(err.to_string()))?;
            if o.json {
                print_json(json!({ "machine": e.id, "share": share }));
            } else {
                println!(
                    "{} shares {} as `{}`{} (from its next start)",
                    e.config.name,
                    share.path,
                    share.name,
                    if share.readonly { ", read-only" } else { "" }
                );
                println!(
                    "  Linux guest:   mount -t virtiofs {} /mnt/host   (then /mnt/host/{})",
                    qorral_core::qemu::SHARE_TAG,
                    share.name
                );
                println!("  Windows guest: install the virtio-fs driver and WinFsp; the shares appear as a drive");
            }
        }
        "remove" | "rm" => {
            let name = o.rest.get(2).ok_or_else(|| Fail::Usage("share remove needs a share name".into()))?;
            let before = c.shares.len();
            c.shares.retain(|s| !s.name.eq_ignore_ascii_case(name));
            if c.shares.len() == before {
                return Err(Fail::NotFound(format!("{} has no share `{name}`", e.config.name)));
            }
            Library::save_config(&e, &c).map_err(|err| Fail::Failed(err.to_string()))?;
            if o.json {
                print_json(json!({ "machine": e.id, "removed": name }));
            } else {
                println!("{} no longer shares `{name}` (from its next start)", e.config.name);
            }
        }
        "cache" => {
            let v = o.rest.get(2).ok_or_else(|| Fail::Usage("share cache needs strict or auto".into()))?;
            c.share_cache = qorral_core::ShareCache::parse(v)
                .ok_or_else(|| Fail::Usage(format!("share cache is strict or auto, not `{v}`")))?;
            Library::save_config(&e, &c).map_err(|err| Fail::Failed(err.to_string()))?;
            if o.json {
                print_json(json!({ "machine": e.id, "cache": c.share_cache.id() }));
            } else {
                println!("{} caches shared folders: {} (from its next start)", e.config.name, c.share_cache.id());
            }
        }
        other => return Err(Fail::Usage(format!("unknown share command `{other}` (add, remove, list, cache)"))),
    }
    Ok(())
}

fn base_cmd(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let sub = o.rest.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => {
            let bases = sandbox::list_bases(lib);
            if o.json {
                let v: Vec<Value> = bases
                    .iter()
                    .map(|b| {
                        json!({
                            "name": b.info.name, "created": b.info.created, "source": b.info.source,
                            "dir": b.dir, "system": model::os_label(&b.info.template),
                            "used_by": sandbox::base_users(lib, &b.info.name),
                        })
                    })
                    .collect();
                print_json(json!({ "bases": v }));
            } else if bases.is_empty() {
                println!("No bases yet. Freeze a stopped machine:  qorral base freeze <vm>");
            } else {
                for b in &bases {
                    let users = sandbox::base_users(lib, &b.info.name);
                    println!(
                        "{:<24} {:<14} from {:<20} used by {}",
                        b.info.name,
                        model::os_label(&b.info.template),
                        b.info.source,
                        if users.is_empty() { "nothing".to_string() } else { users.join(", ") }
                    );
                }
            }
            Ok(())
        }
        "freeze" => {
            let want = o.rest.get(1).ok_or_else(|| Fail::Usage("base freeze needs a machine".into()))?;
            let vm = find_named(lib, want)?;
            if sandbox::marker(&vm).is_some() {
                return Err(Fail::Failed("a sandbox cannot be frozen; keep it first (sandbox keep)".into()));
            }
            let q = qemu()?;
            let b = sandbox::freeze(lib, &q, &vm, o.val("--name")).map_err(Fail::Failed)?;
            if o.json {
                print_json(json!({ "base": { "name": b.info.name, "dir": b.dir, "source": b.info.source } }));
            } else {
                println!("Froze {} into base {} ({})", vm.config.name, b.info.name, b.dir.display());
                println!("Start a sandbox:  qorral sandbox run {}", b.info.name);
            }
            Ok(())
        }
        "delete" | "rm" => {
            let name = o.rest.get(1).ok_or_else(|| Fail::Usage("base delete needs a name".into()))?;
            let b = sandbox::find_base(lib, name).ok_or_else(|| Fail::NotFound(format!("no base `{name}`")))?;
            if !o.flag("--yes") {
                return Err(Fail::Usage(format!(
                    "this deletes base {} and its disk in {}; add --yes to confirm",
                    b.info.name,
                    b.dir.display()
                )));
            }
            sandbox::delete_base(lib, &b).map_err(Fail::Failed)?;
            if o.json {
                print_json(json!({ "deleted": b.info.name }));
            } else {
                println!("Deleted base {}", b.info.name);
            }
            Ok(())
        }
        other => Err(Fail::Usage(format!("unknown base command `{other}` (freeze, list, delete)"))),
    }
}

fn sandbox_cmd(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let sub = o.rest.first().map(String::as_str).unwrap_or("list");
    let finished = sandbox::sweep(lib);
    match sub {
        "list" | "ls" => {
            let all = sandbox::sandboxes(lib).list();
            if o.json {
                print_json(json!({
                    "sandboxes": all.iter().map(entry_json).collect::<Vec<_>>(),
                    "cleaned_up": finished.iter().map(|(id, _)| id).collect::<Vec<_>>(),
                }));
                return Ok(());
            }
            for (id, r) in &finished {
                match r {
                    Ok(Finished::Deleted) => println!("cleaned up {id}"),
                    Ok(Finished::Kept(m)) => println!("kept {id} as machine {m}"),
                    Err(e) => eprintln!("qorral: {id}: {e}"),
                }
            }
            if all.is_empty() {
                println!("No sandboxes running. Start one:  qorral sandbox run <base>");
            }
            for e in &all {
                let m = sandbox::marker(e).unwrap_or_default();
                println!(
                    "{:<28} {:<8} base {:<20} {}{}",
                    e.id,
                    state_name(e),
                    m.base,
                    e.config.network.label(),
                    if m.keep { "  (kept when it ends)" } else { "" }
                );
            }
            Ok(())
        }
        "run" | "start" => sandbox_run(lib, o),
        "stop" => {
            let id = o.rest.get(1).ok_or_else(|| Fail::Usage("sandbox stop needs an id".into()))?;
            let e = sandbox_entry(lib, id)?;
            let force = o.flag("--force");
            let timeout =
                Duration::from_secs(o.num("--timeout").map_err(Fail::Usage)?.unwrap_or(if force { 10 } else { 90 }));
            if supervisor::stop(&e, force, timeout).map_err(Fail::Failed)? == State::Running {
                return Err(Fail::Timeout(format!(
                    "{} is still running after {} s (the guest ignored the shutdown request; use --force)",
                    e.id,
                    timeout.as_secs()
                )));
            }
            // The reaper finishes it too; whoever comes first wins.
            let r = match sandbox::finish(lib, &Library::load_dir(&e.dir)) {
                Err(_) if !e.dir.exists() => Ok(Finished::Deleted),
                r => r,
            };
            let r = r.map_err(Fail::Failed)?;
            if o.json {
                print_json(json!({ "sandbox": e.id, "result": finished_json(&r) }));
            } else {
                match r {
                    Finished::Deleted => println!("Ended and deleted {}", e.id),
                    Finished::Kept(m) => println!("Ended {}; kept as machine {m}", e.id),
                }
            }
            Ok(())
        }
        "keep" => {
            let id = o.rest.get(1).ok_or_else(|| Fail::Usage("sandbox keep needs an id".into()))?;
            let e = sandbox_entry(lib, id)?;
            sandbox::keep(&e).map_err(Fail::Failed)?;
            if o.json {
                print_json(json!({ "sandbox": e.id, "keep": true }));
            } else {
                println!("{} becomes a normal machine when it ends", e.id);
            }
            Ok(())
        }
        other => Err(Fail::Usage(format!("unknown sandbox command `{other}` (run, list, stop, keep)"))),
    }
}

fn finished_json(r: &Finished) -> Value {
    match r {
        Finished::Deleted => json!({ "deleted": true }),
        Finished::Kept(id) => json!({ "deleted": false, "machine": id }),
    }
}

fn sandbox_entry(lib: &Library, id: &str) -> Result<VmEntry, Fail> {
    sandbox::sandboxes(lib)
        .list()
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| Fail::NotFound(format!("no sandbox `{id}`")))
}

fn sandbox_run(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let mut profile = match o.val("--profile") {
        Some(p) => sandbox::Profile::load(Path::new(p)).map_err(Fail::Usage)?,
        None => sandbox::Profile::default(),
    };
    // Command-line options override the profile.
    if let Some(b) = o.rest.get(1) {
        profile.base = b.clone();
    }
    if profile.base.is_empty() {
        return Err(Fail::Usage("sandbox run needs a base (or --profile FILE)".into()));
    }
    let s = &mut profile.settings;
    if let Some(n) = network_opt(o)? {
        s.network = Some(n);
    }
    s.forwards.extend(o.vals("--forward"));
    s.qemu_args.extend(o.vals("--qemu-arg"));
    s.shares.extend(o.vals("--share"));
    if let Some(n) = o.num("--cpus").map_err(Fail::Usage)? {
        s.cpus = Some(n.clamp(1, 256) as u32);
    }
    if let Some(m) = o.val("--memory") {
        s.memory_mib = Some(parse_memory(m).map_err(Fail::Usage)?);
    }
    if !o.tail.is_empty() {
        s.startup = o.tail.clone();
    }
    if let Some(m) = o.num("--minutes").map_err(Fail::Usage)? {
        profile.minutes = Some(m);
    }
    if o.flag("--window") {
        profile.window = true;
    }
    let base = sandbox::find_base(lib, &profile.base)
        .ok_or_else(|| Fail::NotFound(format!("no base `{}` (see qorral base list)", profile.base)))?;
    let q = qemu()?;
    let e = sandbox::create_sandbox(lib, &q, &base, &profile.settings).map_err(Fail::Failed)?;
    let opts = supervisor::StartOptions {
        display: if profile.window { Display::Window } else { Display::Headless },
        minutes: profile.minutes,
        ..supervisor::StartOptions::default()
    };
    let run = match supervisor::start(lib, &q, &e, &opts) {
        Ok(r) => r,
        Err(err) => {
            let _ = sandbox::finish(lib, &Library::load_dir(&e.dir));
            return Err(Fail::Failed(err));
        }
    };
    // The reaper deletes the sandbox when QEMU is gone and enforces --minutes.
    spawn_helper("_reap", &e.id, o.val("--home"));
    if o.json {
        print_json(json!({ "sandbox": entry_json(&Library::load_dir(&e.dir)), "pid": run.pid }));
    } else {
        println!(
            "Started sandbox {} on {} (pid {}, {}){}",
            e.id,
            base.info.name,
            run.pid,
            e.config.network.label(),
            profile.minutes.map_or(String::new(), |m| format!(", ends in {m} min"))
        );
        println!("It is deleted when it ends.  qorral sandbox stop {}   |   qorral sandbox keep {}", e.id, e.id);
    }
    Ok(())
}

/// `qorral _reap <id>`: detached; runs the startup command, then deletes
/// the sandbox when its QEMU is gone.
fn reap_cmd(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let id = o.rest.first().ok_or_else(|| Fail::Usage("_reap needs an id".into()))?.clone();
    let e = sandbox_entry(lib, &id)?;
    let startup = sandbox::marker(&e).map(|m| m.startup).unwrap_or_default();
    if let (false, Some(run)) = (startup.is_empty(), e.run.clone()) {
        std::thread::spawn(move || {
            // Waits for the guest to boot; output goes nowhere (detached).
            if let Ok(mut a) = GuestAgent::wait_connect(run.agent_port, Duration::from_secs(15 * 60)) {
                let _ = a.exec(&startup, Duration::from_secs(5));
            }
        });
    }
    sandbox::reap(lib, &id).map(|_| ()).map_err(Fail::Failed)
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

    #[test]
    fn name_equals_value_works_and_unknown_options_are_refused() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = parse(args(&["vm", "--qemu-arg=-d", "--qemu-arg", "guest_errors", "--minutes=5", "--force"])).unwrap();
        assert_eq!(o.vals("--qemu-arg"), ["-d", "guest_errors"]);
        assert_eq!(o.val("--minutes"), Some("5"));
        assert!(o.flag("--force"));
        // A value may itself contain `=`.
        assert_eq!(parse(args(&["--qemu-arg=-global=x.y=1"])).unwrap().vals("--qemu-arg"), ["-global=x.y=1"]);
        assert!(parse(args(&["vm", "--qemu-args", "x"])).unwrap_err().contains("unknown option `--qemu-args`"));
        assert!(parse(args(&["vm", "--force=yes"])).unwrap_err().contains("takes no value"));
        assert!(parse(args(&["vm", "--minutes="])).unwrap().val("--minutes") == Some(""));
    }
}
