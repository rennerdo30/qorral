//! Where VMs live: one folder per VM under the library root, each with
//! `vm.json` (settings) and, while QEMU runs, `run.json` (PID and ports).

use crate::qemu::sanitize;
use crate::vm::VmConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What Qorral recorded when it started QEMU for this VM.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunRecord {
    pub pid: u32,
    pub qmp_port: u16,
    pub agent_port: u16,
    /// Unix seconds.
    pub started: u64,
    /// Unix seconds; 0 = no time limit.
    pub deadline: u64,
    /// Started with QEMU's own window.
    pub window: bool,
    /// gdb stub port, when started with `--gdb`.
    pub gdb_port: Option<u16>,
}

#[derive(Clone, Debug)]
pub struct VmEntry {
    /// Folder name, stable for the VM's lifetime.
    pub id: String,
    pub dir: PathBuf,
    pub config: VmConfig,
    pub run: Option<RunRecord>,
    /// `vm.json` was unreadable; the entry is listed so the user can repair or delete it.
    pub error: Option<String>,
}

pub struct Library {
    pub root: PathBuf,
}

impl Library {
    /// `QORRAL_HOME` overrides the location (tests, development).
    pub fn default_root() -> PathBuf {
        if let Some(p) = std::env::var_os("QORRAL_HOME") {
            return PathBuf::from(p).join("vms");
        }
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        };
        base.unwrap_or_else(|| PathBuf::from(".")).join("Qorral").join("vms")
    }

    pub fn new(root: impl Into<PathBuf>) -> Library {
        Library { root: root.into() }
    }

    /// Every VM folder, sorted by name. A missing root is an empty library.
    pub fn list(&self) -> Vec<VmEntry> {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return Vec::new() };
        let mut out: Vec<VmEntry> =
            rd.flatten().filter(|e| e.path().join("vm.json").is_file()).map(|e| Self::load_dir(&e.path())).collect();
        out.sort_by_key(|v| v.config.name.to_lowercase());
        out
    }

    pub fn load_dir(dir: &Path) -> VmEntry {
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (config, error) = match std::fs::read_to_string(dir.join("vm.json")) {
            Ok(t) => match VmConfig::from_json(&t) {
                Ok(c) => (c, None),
                Err(e) => (VmConfig { name: id.clone(), ..VmConfig::default() }, Some(e)),
            },
            Err(e) => (VmConfig { name: id.clone(), ..VmConfig::default() }, Some(format!("vm.json: {e}"))),
        };
        let run = std::fs::read_to_string(dir.join("run.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
        VmEntry { id, dir: dir.to_path_buf(), config, run, error }
    }

    /// Creates the folder and `vm.json`; returns the new VM's id. Never
    /// overwrites: a taken name gets a numbered folder.
    pub fn create(&self, config: &VmConfig) -> std::io::Result<VmEntry> {
        std::fs::create_dir_all(&self.root)?;
        let base = sanitize(&config.name);
        let mut id = base.clone();
        let mut n = 2;
        let dir = loop {
            let d = self.root.join(&id);
            match std::fs::create_dir(&d) {
                Ok(()) => break d,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    id = format!("{base}-{n}");
                    n += 1;
                }
                Err(e) => return Err(e),
            }
        };
        std::fs::write(dir.join("vm.json"), config.to_json())?;
        Ok(Self::load_dir(&dir))
    }

    pub fn save_run(entry: &VmEntry, run: Option<&RunRecord>) -> std::io::Result<()> {
        let p = entry.dir.join("run.json");
        match run {
            Some(r) => std::fs::write(p, serde_json::to_string_pretty(r).unwrap_or_default()),
            None => match std::fs::remove_file(p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qorral-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn create_never_overwrites_and_lists_sorted_with_broken_entries() {
        let root = temp_root("lib");
        let lib = Library::new(&root);
        assert!(lib.list().is_empty(), "a missing root is an empty library");
        let a = lib.create(&VmConfig { name: "Zeta".into(), ..VmConfig::default() }).unwrap();
        let b = lib.create(&VmConfig { name: "zeta".into(), cpus: 2, ..VmConfig::default() }).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("zeta", "zeta-2"));
        lib.create(&VmConfig { name: "Alpha".into(), ..VmConfig::default() }).unwrap();
        std::fs::create_dir_all(root.join("broken")).unwrap();
        std::fs::write(root.join("broken/vm.json"), "{oops").unwrap();
        let names: Vec<_> = lib.list().into_iter().map(|e| (e.config.name, e.error.is_some())).collect();
        assert_eq!(names[0], ("Alpha".to_string(), false));
        assert!(names.contains(&("broken".to_string(), true)));
        assert_eq!(names.len(), 4);
        let run = RunRecord { pid: 42, qmp_port: 44400, ..RunRecord::default() };
        Library::save_run(&a, Some(&run)).unwrap();
        assert_eq!(Library::load_dir(&a.dir).run, Some(run));
        Library::save_run(&a, None).unwrap();
        Library::save_run(&a, None).unwrap();
        assert_eq!(Library::load_dir(&a.dir).run, None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
