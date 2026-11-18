//! A virtual machine's settings, stored as `vm.json` in its own folder.

use serde::{Deserialize, Serialize};

/// Guest firmware. UEFI is the default for every modern OS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    #[default]
    Uefi,
    Bios,
}

/// How the system disk is attached. NVMe works with Windows' inbox driver
/// during setup; virtio-blk is faster for Linux guests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskBus {
    #[default]
    Virtio,
    Nvme,
}

/// Guest graphics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuMode {
    /// Plain virtio VGA, rendered in software by the guest.
    #[default]
    Basic,
    /// OpenGL passed to the host GPU (virgl).
    Opengl,
    /// Vulkan passed to the host GPU (Venus).
    Vulkan,
}

impl GpuMode {
    pub fn label(self) -> &'static str {
        match self {
            GpuMode::Basic => "Basic display",
            GpuMode::Opengl => "OpenGL",
            GpuMode::Vulkan => "Vulkan",
        }
    }
    /// Accelerated modes render through a GL display context on the host.
    pub fn accelerated(self) -> bool {
        self != GpuMode::Basic
    }
}

/// The guest's network. Port forwards apply to every mode except `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// No network card at all.
    None,
    /// User-mode NAT that reaches neither the internet nor the host; only
    /// explicit port forwards connect.
    Isolated,
    /// User-mode NAT to the internet, but the guest cannot reach services
    /// on the host's loopback (`127.0.0.1`). Services the host offers on
    /// its LAN addresses stay reachable, as from any machine on that LAN.
    Internet,
    /// Plain user-mode NAT, host loopback included (QEMU's default).
    #[default]
    Nat,
}

impl NetworkMode {
    pub const ALL: [NetworkMode; 4] =
        [NetworkMode::None, NetworkMode::Isolated, NetworkMode::Internet, NetworkMode::Nat];

    pub fn id(self) -> &'static str {
        match self {
            NetworkMode::None => "none",
            NetworkMode::Isolated => "isolated",
            NetworkMode::Internet => "internet",
            NetworkMode::Nat => "nat",
        }
    }

    pub fn parse(s: &str) -> Option<NetworkMode> {
        NetworkMode::ALL.into_iter().find(|m| m.id().eq_ignore_ascii_case(s.trim()))
    }

    pub fn label(self) -> &'static str {
        match self {
            NetworkMode::None => "No network",
            NetworkMode::Isolated => "Isolated (forwards only)",
            NetworkMode::Internet => "Internet, host blocked",
            NetworkMode::Nat => "Network (NAT)",
        }
    }
}

/// The guest's sound card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// No sound card.
    Off,
    /// Guest sound plays on the host's default output; no microphone.
    #[default]
    Output,
    /// Output, and the guest may record the host's default microphone.
    Mic,
}

impl AudioMode {
    pub const ALL: [AudioMode; 3] = [AudioMode::Off, AudioMode::Output, AudioMode::Mic];

    pub fn id(self) -> &'static str {
        match self {
            AudioMode::Off => "off",
            AudioMode::Output => "on",
            AudioMode::Mic => "mic",
        }
    }

    /// `off`, `on` (or `output`), `mic`.
    pub fn parse(s: &str) -> Option<AudioMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "output" => Some(AudioMode::Output),
            t => AudioMode::ALL.into_iter().find(|m| m.id() == t),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AudioMode::Off => "No sound",
            AudioMode::Output => "Sound",
            AudioMode::Mic => "Sound and microphone",
        }
    }
}

/// A host folder the guest sees through virtio-fs as `<mount>/<name>`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Share {
    pub name: String,
    pub path: String,
    pub readonly: bool,
}

impl Share {
    /// `C:\Users\me\Docs` → `Docs`; `D:\` → `D`. Letters, digits, `-`, `_`, `.`.
    pub fn name_for(path: &str) -> String {
        let last = path.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or("");
        let last = last.trim_end_matches(':');
        let n: String = last
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
            .collect();
        let n = n.trim_matches(['-', '.']).to_string();
        if n.is_empty() { "share".into() } else { n }
    }

    /// `PATH`, `PATH:ro` or `PATH:rw` (a drive letter colon is part of the path).
    pub fn parse(spec: &str, default_readonly: bool) -> Share {
        let (path, readonly) = match spec.rsplit_once(':') {
            Some((p, "ro")) if !p.is_empty() => (p, true),
            Some((p, "rw")) if !p.is_empty() => (p, false),
            _ => (spec, default_readonly),
        };
        Share { name: Share::name_for(path), path: path.to_string(), readonly }
    }

    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !name.starts_with('.')
    }
}

/// How much the guest may cache shared folders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareCache {
    /// Every change on the host is visible at once; each access asks the host.
    Strict,
    /// Attributes cached for a second and file data in the guest's page
    /// cache (dropped when the file changes): much faster with many small
    /// files; host-side changes show up within a second. The default, as
    /// virtiofsd's `cache=auto`.
    #[default]
    Auto,
}

impl ShareCache {
    pub fn id(self) -> &'static str {
        match self {
            ShareCache::Strict => "strict",
            ShareCache::Auto => "auto",
        }
    }
    pub fn parse(s: &str) -> Option<ShareCache> {
        [ShareCache::Strict, ShareCache::Auto].into_iter().find(|c| c.id().eq_ignore_ascii_case(s.trim()))
    }
}

/// The guest family. It picks defaults and the logo; it never limits what
/// the user may change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    #[default]
    Linux,
    Windows,
    Bsd,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmConfig {
    /// Shown name; the folder name is derived from it once and never changes.
    pub name: String,
    pub os: OsKind,
    /// Catalog entry this VM was made from, e.g. `fedora-43-workstation`.
    pub preset: Option<String>,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub firmware: Firmware,
    pub disk_bus: DiskBus,
    pub gpu: GpuMode,
    /// Install medium; removed by the user (or the installer flow) after setup.
    pub iso: Option<String>,
    pub network: NetworkMode,
    /// Port forwards in QEMU's `hostfwd` syntax, e.g. `tcp:127.0.0.1:2222-:22`.
    pub port_forwards: Vec<String>,
    /// Host folders shared with the guest (virtio-fs, tag `qorral`).
    pub shares: Vec<Share>,
    pub share_cache: ShareCache,
    /// The sound card (default: sound out, no microphone).
    pub audio: AudioMode,
    /// Extra QEMU arguments for power users, appended last.
    pub extra_args: Vec<String>,
    /// Disk image outside the machine folder (adopted machines). Default:
    /// `disk.qcow2` inside the folder.
    pub disk_path: Option<String>,
    /// UEFI variable store outside the machine folder. Default:
    /// `efivars.qcow2` inside the folder.
    pub efivars_path: Option<String>,
    /// The frozen base this machine's disk builds on (`qorral base`). A base
    /// in use cannot be deleted.
    pub base: Option<String>,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig {
            name: String::new(),
            os: OsKind::Linux,
            preset: None,
            cpus: 4,
            memory_mib: 8192,
            disk_gib: 64,
            firmware: Firmware::Uefi,
            disk_bus: DiskBus::Virtio,
            gpu: GpuMode::Basic,
            iso: None,
            network: NetworkMode::Nat,
            port_forwards: Vec::new(),
            shares: Vec::new(),
            share_cache: ShareCache::Auto,
            audio: AudioMode::Output,
            extra_args: Vec::new(),
            disk_path: None,
            efivars_path: None,
            base: None,
        }
    }
}

impl VmConfig {
    pub fn to_json(&self) -> String {
        // Serialising plain data with string keys cannot fail.
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Parses `vm.json`. Unknown fields are ignored and missing ones take
    /// their defaults, so files from older and newer versions both load.
    pub fn from_json(text: &str) -> Result<VmConfig, String> {
        serde_json::from_str(text).map_err(|e| format!("vm.json: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_and_tolerates_missing_and_unknown_fields() {
        let vm = VmConfig {
            name: "Fedora".into(),
            gpu: GpuMode::Vulkan,
            port_forwards: vec!["tcp:127.0.0.1:2222-:22".into()],
            ..VmConfig::default()
        };
        assert_eq!(VmConfig::from_json(&vm.to_json()).unwrap(), vm);
        let old = VmConfig::from_json(r#"{"name":"x","cpus":2,"future_field":true}"#).unwrap();
        assert_eq!((old.name.as_str(), old.cpus, old.memory_mib), ("x", 2, 8192));
        assert!(VmConfig::from_json("{not json").is_err());
        assert_eq!(old.network, NetworkMode::Nat, "older files keep plain NAT");
    }

    #[test]
    fn share_specs_and_names() {
        let s = Share::parse(r"C:\Users\me\My Docs:ro", false);
        assert_eq!((s.name.as_str(), s.path.as_str(), s.readonly), ("My-Docs", r"C:\Users\me\My Docs", true));
        let s = Share::parse(r"D:\", true);
        assert_eq!((s.name.as_str(), s.path.as_str(), s.readonly), ("D", r"D:\", true), "drive colon is no suffix");
        assert!(!Share::parse("/srv/data:rw", true).readonly);
        assert!(Share::valid_name("work-1.2") && !Share::valid_name("../x") && !Share::valid_name(".hidden"));
    }

    #[test]
    fn network_modes_parse_by_id() {
        for m in NetworkMode::ALL {
            assert_eq!(NetworkMode::parse(m.id()), Some(m));
        }
        assert_eq!(NetworkMode::parse(" Isolated "), Some(NetworkMode::Isolated));
        assert_eq!(NetworkMode::parse("bridge"), None);
    }
}
