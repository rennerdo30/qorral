//! `qorral <command>`: everything the GUI does, for scripts and other apps.
//!
//! Contract (HG-12):
//! - `--json` prints one JSON document on stdout; its schema has `"schema": 1`.
//! - Exit codes: 0 done, 1 failed, 2 usage error, 3 no such machine.
//! - Errors go to stderr. Nothing prompts; destructive commands need `--yes`.

use crate::model;
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
  create <name> [--iso FILE] [--preset ID] [--cpus N] [--memory GB] [--disk GB]
                [--gpu basic|opengl|vulkan|venus]
  start <vm> [--window] [--minutes N]   start (headless unless --window); N = time limit
  stop <vm> [--force] [--timeout S]     ACPI shutdown, or end QEMU at once
  screenshot <vm> <out.png>             the guest screen as PNG
  keys <vm> <combo>...                  press keys, e.g. ctrl-alt-delete ret
  delete <vm> --yes                     delete a stopped machine and its disk
  host                                  the QEMU build and accelerator in use
  presets                               systems with tuned defaults

options: --json (machine-readable output), --home DIR (library location)
exit codes: 0 done, 1 failed, 2 usage error, 3 no such machine
";

struct Opts {
    json: bool,
    rest: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

/// Options that take a value.
const VALUED: [&str; 10] =
    ["--iso", "--preset", "--cpus", "--memory", "--disk", "--gpu", "--minutes", "--timeout", "--home", "--name"];

fn parse(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts { json: false, rest: Vec::new(), flags: Vec::new() };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--json" {
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
    fn num(&self, n: &str) -> Result<Option<u64>, String> {
        self.val(n).map(|v| v.parse::<u64>().map_err(|_| format!("{n} needs a number"))).transpose()
    }
}

enum Fail {
    Usage(String),
    NotFound(String),
    Failed(String),
}

pub fn run(args: Vec<String>) -> i32 {
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
        "stop" => stop(&lib, &o),
        "screenshot" => screenshot(&lib, &o),
        "keys" => keys(&lib, &o),
        "delete" | "rm" => delete(&lib, &o),
        "host" => host(&o),
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
    if let Some(n) = o.num("--memory").map_err(Fail::Usage)? {
        c.memory_mib = (n.clamp(1, 1024) * 1024) as u32;
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

fn start(lib: &Library, o: &Opts) -> Result<(), Fail> {
    let e = find(lib, o)?;
    let q = qemu()?;
    let minutes = o.num("--minutes").map_err(Fail::Usage)?;
    let display = if o.flag("--window") { Display::Window } else { Display::Headless };
    let run = supervisor::start(lib, &q, &e, display, minutes).map_err(Fail::Failed)?;
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
        return Err(Fail::Failed(format!(
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

fn host(o: &Opts) -> Result<(), Fail> {
    let q = QemuInstall::find();
    let v = json!({
        "accel": qorral_core::qemu::host_accel(),
        "qemu": q.as_ref().map(|q| json!({
            "dir": q.dir,
            "version": q.version(),
            "displays": q.displays(),
            "uefi": q.firmware_code().is_some(),
        })),
        "library": Library::default_root(),
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
        println!("library      {}", Library::default_root().display());
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
