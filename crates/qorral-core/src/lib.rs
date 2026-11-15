//! Qorral's core: what a virtual machine is, how it becomes a QEMU command
//! line, how Qorral talks to a running QEMU (QMP) and where VMs live on disk.
//!
//! Nothing here touches the UI. Everything that blocks (process starts, sockets,
//! disk images) is called from worker threads by the app.

pub mod guest;
pub mod host;
pub mod image;
pub mod library;
pub mod process;
pub mod qemu;
pub mod qmp;
pub mod sandbox;
pub mod supervisor;
pub mod vm;

pub use library::{Library, VmEntry};
pub use vm::{DiskBus, Firmware, GpuMode, NetworkMode, OsKind, Share, VmConfig};
