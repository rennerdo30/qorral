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
    /// Port forwards in QEMU's `hostfwd` syntax, e.g. `tcp:127.0.0.1:2222-:22`.
    pub port_forwards: Vec<String>,
    /// Extra QEMU arguments for power users, appended last.
    pub extra_args: Vec<String>,
    /// Disk image outside the machine folder (adopted machines). Default:
    /// `disk.qcow2` inside the folder.
    pub disk_path: Option<String>,
    /// UEFI variable store outside the machine folder. Default:
    /// `efivars.qcow2` inside the folder.
    pub efivars_path: Option<String>,
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
            port_forwards: Vec::new(),
            extra_args: Vec::new(),
            disk_path: None,
            efivars_path: None,
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
    }
}
