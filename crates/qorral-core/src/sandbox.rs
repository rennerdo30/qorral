//! Frozen bases and disposable sandboxes (HG-3).
//!
//! Layout next to the machine library (`<home>/vms`):
//! - `<home>/bases/<name>/`: `base.json`, a read-only `disk.qcow2` and the
//!   UEFI store. A base never changes after it is frozen.
//! - `<home>/sandboxes/<id>/`: an ordinary machine folder (`vm.json`,
//!   `run.json`, an overlay disk on the base) plus `sandbox.json`, the
//!   marker that allows deleting it. Being a plain library, every machine
//!   command (stop, exec, screenshot, keys) works on sandboxes too.
//!
//! A sandbox is deleted when its QEMU is gone: the reaper (`qorral _reap`)
//! waits for the exit, and [`sweep`] catches anything a crash left behind.
//! Only a folder directly inside the sandbox root that carries the marker is
//! ever removed. `keep` turns a sandbox into a normal machine instead.

use crate::host::QemuInstall;
use crate::library::{Library, VmEntry};
use crate::process::{is_our_qemu, run_with_timeout, wait_gone};
use crate::qemu::{VmPaths, sanitize};
use crate::supervisor::{self, State, now};
use crate::vm::{NetworkMode, VmConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MARKER: &str = "sandbox.json";
/// A sandbox folder without a running QEMU for this long is left over.
const ORPHAN_GRACE_SECS: u64 = 10 * 60;

/// `<home>`: the folder that holds `vms`, `bases` and `sandboxes`.
pub fn home(lib: &Library) -> PathBuf {
    lib.root.parent().map_or_else(|| lib.root.clone(), Path::to_path_buf)
}

pub fn bases_root(lib: &Library) -> PathBuf {
    home(lib).join("bases")
}

/// The sandboxes, as their own machine library.
pub fn sandboxes(lib: &Library) -> Library {
    Library::new(home(lib).join("sandboxes"))
}

/// `base.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BaseInfo {
    pub name: String,
    /// Unix seconds.
    pub created: u64,
    /// The machine it was frozen from.
    pub source: String,
    /// Hardware and settings every sandbox starts with (its own disk aside).
    pub template: VmConfig,
    /// The UEFI store is part of the base (UEFI machines).
    pub has_efivars: bool,
}

#[derive(Clone, Debug)]
pub struct Base {
    pub dir: PathBuf,
    pub info: BaseInfo,
}

impl Base {
    pub fn disk(&self) -> PathBuf {
        self.dir.join("disk.qcow2")
    }
    pub fn efivars(&self) -> Option<PathBuf> {
        self.info.has_efivars.then(|| self.dir.join("efivars.qcow2"))
    }
}

pub fn list_bases(lib: &Library) -> Vec<Base> {
    let Ok(rd) = std::fs::read_dir(bases_root(lib)) else { return Vec::new() };
    let mut out: Vec<Base> = rd
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("base.json")).ok()?;
            let info: BaseInfo = serde_json::from_str(&text).ok()?;
            Some(Base { dir: e.path(), info })
        })
        .collect();
    out.sort_by_key(|b| b.info.name.to_lowercase());
    out
}

pub fn find_base(lib: &Library, name: &str) -> Option<Base> {
    list_bases(lib).into_iter().find(|b| b.info.name.eq_ignore_ascii_case(name))
}

fn qemu_img(q: &QemuInstall, args: &[&str], what: &str, timeout: Duration) -> Result<(), String> {
    let mut c = Command::new(q.img());
    c.args(args);
    let out = run_with_timeout(c, timeout)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// A new qcow2 overlay at `overlay` on top of `backing` (absolute path).
fn create_overlay(q: &QemuInstall, backing: &Path, overlay: &Path) -> Result<(), String> {
    let b = backing.display().to_string();
    let o = overlay.display().to_string();
    qemu_img(
        q,
        &["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", &b, &o],
        "creating the overlay",
        Duration::from_secs(60),
    )
}

fn set_readonly(p: &Path, readonly: bool) {
    if let Ok(meta) = std::fs::metadata(p) {
        let mut perm = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(readonly);
        let _ = std::fs::set_permissions(p, perm);
    }
}

/// Freezes a stopped machine into a read-only base.
///
/// - Disk inside the machine folder: it moves into the base (no copy) and
///   the machine continues on a new overlay on top of it.
/// - Adopted disk elsewhere: it is copied (flattened), never moved, so other
///   tools that use it keep working; the machine itself is unchanged.
pub fn freeze(lib: &Library, q: &QemuInstall, vm: &VmEntry, name: Option<&str>) -> Result<Base, String> {
    if supervisor::state(vm) == State::Running {
        return Err(format!("{} is running; stop it before freezing", vm.config.name));
    }
    if let Some(e) = &vm.error {
        return Err(format!("{} has a broken vm.json: {e}", vm.config.name));
    }
    let name = sanitize(name.unwrap_or(&vm.config.name));
    let root = bases_root(lib);
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    // A taken name gets a numbered folder, so every freeze is its own version.
    let (mut id, mut n) = (name.clone(), 2);
    let dir = loop {
        let d = root.join(&id);
        match std::fs::create_dir(&d) {
            Ok(()) => break d,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                id = format!("{name}-{n}");
                n += 1;
            }
            Err(e) => return Err(format!("cannot create {}: {e}", d.display())),
        }
    };
    let result = freeze_into(q, vm, &id, &dir);
    if result.is_err() {
        // Nothing of a failed freeze stays behind (the machine was restored).
        for f in ["disk.qcow2", "efivars.qcow2"] {
            set_readonly(&dir.join(f), false);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

fn freeze_into(q: &QemuInstall, vm: &VmEntry, id: &str, dir: &Path) -> Result<Base, String> {
    let paths = VmPaths::for_vm(&vm.dir, &vm.config);
    if !paths.disk.is_file() {
        return Err(format!("the disk image is missing: {}", paths.disk.display()));
    }
    let base_disk = dir.join("disk.qcow2");
    let inside = paths.disk.parent() == Some(vm.dir.as_path());
    if inside {
        std::fs::rename(&paths.disk, &base_disk).map_err(|e| format!("cannot move the disk into the base: {e}"))?;
        if let Err(e) = create_overlay(q, &std::path::absolute(&base_disk).unwrap_or(base_disk.clone()), &paths.disk) {
            // Put the machine back exactly as it was.
            let _ = std::fs::rename(&base_disk, &paths.disk);
            return Err(e);
        }
    } else {
        let src = paths.disk.display().to_string();
        let dst = base_disk.display().to_string();
        qemu_img(q, &["convert", "-q", "-O", "qcow2", &src, &dst], "copying the disk", Duration::from_secs(4 * 3600))?;
    }
    let has_efivars = paths.efivars.is_file();
    if has_efivars {
        std::fs::copy(&paths.efivars, dir.join("efivars.qcow2"))
            .map_err(|e| format!("cannot copy the UEFI store: {e}"))?;
    }
    let mut template = vm.config.clone();
    template.name = id.to_string();
    template.iso = None;
    template.disk_path = None;
    template.efivars_path = None;
    template.base = Some(id.to_string());
    let info = BaseInfo { name: id.to_string(), created: now(), source: vm.id.clone(), template, has_efivars };
    let json = serde_json::to_string_pretty(&info).unwrap_or_default();
    std::fs::write(dir.join("base.json"), json).map_err(|e| format!("cannot write base.json: {e}"))?;
    set_readonly(&base_disk, true);
    set_readonly(&dir.join("efivars.qcow2"), true);
    if inside {
        let mut c = vm.config.clone();
        c.base = Some(id.to_string());
        Library::save_config(vm, &c).map_err(|e| format!("cannot update {}'s vm.json: {e}", vm.config.name))?;
    }
    Ok(Base { dir: dir.to_path_buf(), info })
}

/// Who still builds on a base (machines and sandboxes).
pub fn base_users(lib: &Library, name: &str) -> Vec<String> {
    let mut users: Vec<String> =
        lib.list().into_iter().filter(|e| e.config.base.as_deref() == Some(name)).map(|e| e.id).collect();
    users.extend(sandboxes(lib).list().into_iter().filter(|e| e.config.base.as_deref() == Some(name)).map(|e| e.id));
    users
}

/// Deletes a base nothing uses any more.
pub fn delete_base(lib: &Library, base: &Base) -> Result<(), String> {
    let users = base_users(lib, &base.info.name);
    if !users.is_empty() {
        return Err(format!("{} is still used by: {}", base.info.name, users.join(", ")));
    }
    if base.dir.parent() != Some(bases_root(lib).as_path()) || !base.dir.join("base.json").is_file() {
        return Err(format!("refusing to delete {}: not a base folder", base.dir.display()));
    }
    for f in ["disk.qcow2", "efivars.qcow2"] {
        set_readonly(&base.dir.join(f), false);
    }
    std::fs::remove_dir_all(&base.dir).map_err(|e| format!("cannot delete {}: {e}", base.dir.display()))
}

/// `sandbox.json`: present means "delete when QEMU is gone".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxMarker {
    pub base: String,
    /// Unix seconds.
    pub created: u64,
    /// Turn into a normal machine instead of deleting it.
    pub keep: bool,
    /// Run through the guest agent once it answers (profiles).
    pub startup: Vec<String>,
}

pub fn marker(entry: &VmEntry) -> Option<SandboxMarker> {
    let text = std::fs::read_to_string(entry.dir.join(MARKER)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_marker(dir: &Path, m: &SandboxMarker) -> Result<(), String> {
    std::fs::write(dir.join(MARKER), serde_json::to_string_pretty(m).unwrap_or_default())
        .map_err(|e| format!("cannot write the sandbox marker: {e}"))
}

/// What a sandbox differs in from its base (everything optional).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    /// Default: no network (sandboxes are locked down unless told otherwise).
    pub network: Option<NetworkMode>,
    pub forwards: Vec<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u32>,
    /// Extra QEMU arguments.
    pub qemu_args: Vec<String>,
    /// Startup command for the guest agent.
    pub startup: Vec<String>,
    /// Host folders granted to the sandbox (`PATH`, `PATH:ro`, `PATH:rw`);
    /// read-only unless marked `:rw`.
    pub shares: Vec<String>,
}

/// A sandbox profile file: one JSON document a team shares to start the
/// same sandbox. Fields: `base` (required), `network`, `forwards`, `cpus`,
/// `memory_mib`, `qemu_args`, `startup`, `minutes`, `window`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub base: String,
    #[serde(flatten)]
    pub settings: SandboxSettings,
    /// Time limit; the sandbox ends (and is deleted) when it passes.
    pub minutes: Option<u64>,
    /// Show QEMU's own window.
    pub window: bool,
}

impl Profile {
    const FIELDS: [&'static str; 10] =
        ["base", "network", "forwards", "cpus", "memory_mib", "qemu_args", "startup", "shares", "minutes", "window"];

    /// Parses a profile. Unknown fields are errors: a typo must not
    /// silently open up a locked-down sandbox.
    pub fn parse(text: &str) -> Result<Profile, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("profile: {e}"))?;
        let obj = v.as_object().ok_or("profile: expected a JSON object")?;
        if let Some(k) = obj.keys().find(|k| !Self::FIELDS.contains(&k.as_str())) {
            return Err(format!("profile: unknown field `{k}` (known: {})", Self::FIELDS.join(", ")));
        }
        let p: Profile = serde_json::from_value(v).map_err(|e| format!("profile: {e}"))?;
        if p.base.trim().is_empty() {
            return Err("profile: `base` is required".into());
        }
        Ok(p)
    }

    pub fn load(path: &Path) -> Result<Profile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text)
    }
}

/// Creates a sandbox (overlay + UEFI store copy), not yet started.
pub fn create_sandbox(lib: &Library, q: &QemuInstall, base: &Base, s: &SandboxSettings) -> Result<VmEntry, String> {
    if !base.disk().is_file() {
        return Err(format!("base {} has no disk image", base.info.name));
    }
    let sb = sandboxes(lib);
    let mut c = base.info.template.clone();
    c.name = format!("{}-{}", base.info.name, short_id());
    c.base = Some(base.info.name.clone());
    c.network = s.network.unwrap_or(NetworkMode::None);
    c.port_forwards = if c.network == NetworkMode::None { Vec::new() } else { s.forwards.clone() };
    if let Some(n) = s.cpus {
        c.cpus = n.clamp(1, 256);
    }
    if let Some(m) = s.memory_mib {
        c.memory_mib = m.max(256);
    }
    c.extra_args.extend(s.qemu_args.iter().cloned());
    // Sandboxes get no folder unless granted, and read-only unless `:rw`.
    c.shares = s.shares.iter().map(|spec| crate::vm::Share::parse(spec, true)).collect();
    supervisor::check_shares(&c.shares)?;
    let entry = sb.create(&c).map_err(|e| format!("cannot create the sandbox folder: {e}"))?;
    let result = (|| {
        write_marker(
            &entry.dir,
            &SandboxMarker { base: base.info.name.clone(), created: now(), keep: false, startup: s.startup.clone() },
        )?;
        let paths = VmPaths::in_dir(&entry.dir);
        create_overlay(q, &std::path::absolute(base.disk()).unwrap_or(base.disk()), &paths.disk)?;
        if let Some(v) = base.efivars() {
            std::fs::copy(&v, &paths.efivars).map_err(|e| format!("cannot copy the UEFI store: {e}"))?;
            set_readonly(&paths.efivars, false);
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(Library::load_dir(&entry.dir)),
        Err(e) => {
            let _ = remove_sandbox_dir(&sb, &entry.dir);
            Err(e)
        }
    }
}

fn short_id() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let x = (t as u64) ^ ((std::process::id() as u64) << 32);
    format!("{:04x}", (x ^ (x >> 16) ^ (x >> 32)) & 0xFFFF)
}

/// Marks a sandbox to become a normal machine when it ends.
pub fn keep(entry: &VmEntry) -> Result<(), String> {
    let mut m = marker(entry).ok_or_else(|| format!("{} is not a sandbox", entry.id))?;
    m.keep = true;
    write_marker(&entry.dir, &m)
}

/// Removes a sandbox folder, but only one directly in the sandbox root
/// that carries the marker.
fn remove_sandbox_dir(sb: &Library, dir: &Path) -> Result<(), String> {
    if dir.parent() != Some(sb.root.as_path()) || !dir.join(MARKER).is_file() {
        return Err(format!("refusing to delete {}: not a sandbox folder", dir.display()));
    }
    // Files can stay locked for a moment after QEMU exits.
    let mut last = String::new();
    for _ in 0..20 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!("cannot delete {}: {last}", dir.display()))
}

/// What happened to a finished sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finished {
    Deleted,
    /// Became a normal machine with this id.
    Kept(String),
}

/// Ends a sandbox whose QEMU is gone: delete it, or move it into the
/// machine library when it was marked `keep`.
pub fn finish(lib: &Library, entry: &VmEntry) -> Result<Finished, String> {
    if supervisor::state(entry) == State::Running {
        return Err(format!("{} is still running", entry.id));
    }
    let sb = sandboxes(lib);
    let m = marker(entry).ok_or_else(|| format!("{} is not a sandbox", entry.id))?;
    if !m.keep {
        remove_sandbox_dir(&sb, &entry.dir)?;
        return Ok(Finished::Deleted);
    }
    std::fs::create_dir_all(&lib.root).map_err(|e| e.to_string())?;
    let (mut id, mut n) = (entry.id.clone(), 2);
    while lib.root.join(&id).exists() {
        id = format!("{}-{n}", entry.id);
        n += 1;
    }
    let _ = Library::save_run(entry, None);
    let _ = std::fs::remove_file(entry.dir.join(MARKER));
    std::fs::rename(&entry.dir, lib.root.join(&id))
        .map_err(|e| format!("cannot move the sandbox into the library: {e}"))?;
    Ok(Finished::Kept(id))
}

/// Deletes (or keeps) every sandbox whose QEMU is gone: crashes, a killed
/// reaper, a host reboot. A folder still being set up gets a grace period.
pub fn sweep(lib: &Library) -> Vec<(String, Result<Finished, String>)> {
    let mut out = Vec::new();
    for e in sandboxes(lib).list() {
        let Some(m) = marker(&e) else { continue };
        let dead = match &e.run {
            Some(r) => !is_our_qemu(r.pid),
            None => now().saturating_sub(m.created) > ORPHAN_GRACE_SECS,
        };
        if dead {
            out.push((e.id.clone(), finish(lib, &e)));
        }
    }
    out
}

/// The reaper: waits until the sandbox's QEMU is gone (enforcing its time
/// limit), then finishes it. Blocking; runs as a detached `qorral _reap`.
pub fn reap(lib: &Library, id: &str) -> Result<Finished, String> {
    let sb = sandboxes(lib);
    let dir = sb.root.join(id);
    loop {
        let e = Library::load_dir(&dir);
        if marker(&e).is_none() {
            return Err(format!("{id} is not a sandbox (any more)"));
        }
        match &e.run {
            Some(r) if is_our_qemu(r.pid) => {
                if supervisor::enforce_deadline(&e) {
                    continue;
                }
                // Wakes at once when QEMU exits; otherwise re-checks the deadline.
                wait_gone(r.pid, Duration::from_secs(5));
            }
            Some(r) => {
                wait_gone(r.pid, Duration::from_secs(30));
                return finish(lib, &Library::load_dir(&dir));
            }
            None => {
                // Not started yet (or the start failed): give it the grace period.
                let m = marker(&e).unwrap_or_default();
                if now().saturating_sub(m.created) > ORPHAN_GRACE_SECS {
                    return finish(lib, &e);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qorral-sbx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn fake_sandbox(lib: &Library, keep: bool, created: u64) -> VmEntry {
        let sb = sandboxes(lib);
        let e = sb.create(&VmConfig { name: "box".into(), base: Some("b".into()), ..VmConfig::default() }).unwrap();
        write_marker(&e.dir, &SandboxMarker { base: "b".into(), created, keep, startup: vec![] }).unwrap();
        std::fs::write(e.dir.join("disk.qcow2"), b"overlay").unwrap();
        Library::load_dir(&e.dir)
    }

    #[test]
    fn finished_sandboxes_are_deleted_or_kept_and_only_marked_folders_go() {
        let home = temp_home("finish");
        let lib = Library::new(home.join("vms"));
        let gone = fake_sandbox(&lib, false, now());
        assert_eq!(finish(&lib, &gone), Ok(Finished::Deleted));
        assert!(!gone.dir.exists());

        let kept = fake_sandbox(&lib, true, now());
        let r = finish(&lib, &kept);
        let Ok(Finished::Kept(id)) = r else { panic!("expected kept, got {r:?}") };
        let moved = lib.root.join(&id);
        assert!(moved.join("disk.qcow2").is_file() && !moved.join(MARKER).exists());
        assert_eq!(lib.list().len(), 1, "the kept sandbox is a normal machine now");

        // A folder without the marker is never deleted.
        let sb = sandboxes(&lib);
        let plain = sb.create(&VmConfig { name: "plain".into(), ..VmConfig::default() }).unwrap();
        assert!(remove_sandbox_dir(&sb, &plain.dir).is_err());
        assert!(plain.dir.is_dir());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sweep_keeps_fresh_setups_and_removes_old_orphans() {
        let home = temp_home("sweep");
        let lib = Library::new(home.join("vms"));
        let fresh = fake_sandbox(&lib, false, now());
        let old = fake_sandbox(&lib, false, now() - ORPHAN_GRACE_SECS - 5);
        // A started sandbox whose QEMU is gone (PID 0 is never a QEMU).
        let crashed = fake_sandbox(&lib, false, now());
        Library::save_run(&crashed, Some(&crate::library::RunRecord { pid: 0, ..Default::default() })).unwrap();
        let swept: Vec<String> = sweep(&lib).into_iter().map(|(id, _)| id).collect();
        assert!(fresh.dir.is_dir(), "a sandbox still being set up stays");
        assert!(!old.dir.exists() && !crashed.dir.exists());
        assert_eq!(swept.len(), 2);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn profiles_parse_and_reject_typos() {
        let p = Profile::parse(
            r#"{"base":"win11","network":"isolated","forwards":["tcp:127.0.0.1:3390-:3389"],
                "minutes":30,"startup":["cmd.exe","/c","ver"]}"#,
        )
        .unwrap();
        assert_eq!(p.base, "win11");
        assert_eq!(p.settings.network, Some(NetworkMode::Isolated));
        assert_eq!(p.minutes, Some(30));
        assert_eq!(p.settings.startup.len(), 3);
        assert!(Profile::parse(r#"{"base":"x","netwrok":"nat"}"#).unwrap_err().contains("netwrok"));
        assert!(Profile::parse(r#"{"network":"nat"}"#).is_err(), "base is required");
        assert!(Profile::parse(r#"{"base":"x","network":"bridge"}"#).is_err());
    }

    #[test]
    fn bases_in_use_cannot_be_deleted() {
        let home = temp_home("base");
        let lib = Library::new(home.join("vms"));
        let dir = bases_root(&lib).join("b");
        std::fs::create_dir_all(&dir).unwrap();
        let info = BaseInfo { name: "b".into(), ..BaseInfo::default() };
        std::fs::write(dir.join("base.json"), serde_json::to_string(&info).unwrap()).unwrap();
        std::fs::write(dir.join("disk.qcow2"), b"base").unwrap();
        set_readonly(&dir.join("disk.qcow2"), true);
        let base = find_base(&lib, "B").expect("found case-insensitively");
        let user = fake_sandbox(&lib, false, now());
        assert!(delete_base(&lib, &base).unwrap_err().contains(&user.id));
        finish(&lib, &user).unwrap();
        delete_base(&lib, &base).expect("unused read-only base deletes");
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
