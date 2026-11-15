//! Qorral's shared-folder file server.
//!
//! QEMU's in-process virtio-fs device hands each guest request (a FUSE
//! message) to this library and gets the reply back in the guest's own
//! buffers. The file system itself is OpenVMM's virtio-fs server (MIT),
//! vendored in [`vfs`], on top of OpenVMM's `fuse` and `lxutil` crates.

pub mod ffi;
pub mod server;
pub mod vfs;

pub use server::{Server, Share};
