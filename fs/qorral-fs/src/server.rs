//! One file server per virtio-fs device: host folders as named shares
//! under one root, answering FUSE requests.

use crate::vfs::{CacheMode, VirtioFs};
use fuse::{ReplySender, Request, RequestReader, Session};
use lxutil::LxVolumeOptions;
use serde::Deserialize;
use std::io::{self, IoSlice, Read};

/// One host folder, visible in the guest as `<mount>/<name>`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Share {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Config {
    shares: Vec<Share>,
    /// "strict" (default) or "auto"; see [`CacheMode`].
    #[serde(default)]
    cache: Option<String>,
    /// Linux semantics: exact-case names and Linux owner/mode stored on the
    /// host (default). false for Windows guests: case-insensitive names, no
    /// Linux metadata.
    #[serde(default)]
    posix: Option<bool>,
}

fn parse_cache(v: Option<&str>) -> Result<CacheMode, String> {
    match v.unwrap_or("strict") {
        "strict" => Ok(CacheMode::Strict),
        "auto" => Ok(CacheMode::Auto),
        other => Err(format!("cache must be strict or auto, not `{other}`")),
    }
}

pub struct Server {
    session: Session,
    /// The same file system, for requests the FUSE parser cannot carry.
    fs: VirtioFs,
    /// Host folders, for the root's free-space figures.
    paths: Vec<String>,
    /// `QORRAL_FS_TRACE=<file>`: one line per request (diagnostics only).
    trace: Option<std::sync::Mutex<std::fs::File>>,
}

impl Server {
    /// `{"shares":[{"name":"docs","path":"C:\\Users\\me\\Documents","readonly":true}]}`
    pub fn from_json(config: &str) -> Result<Server, String> {
        let c: Config = serde_json::from_str(config).map_err(|e| format!("share config: {e}"))?;
        Server::with_options(&c.shares, parse_cache(c.cache.as_deref())?, c.posix.unwrap_or(true))
    }

    /// Every share is a child of one synthetic, read-only root, so a guest
    /// mounts the device once and sees all shares by name.
    pub fn new(shares: &[Share]) -> Result<Server, String> {
        Server::with_cache(shares, CacheMode::Strict)
    }

    pub fn with_cache(shares: &[Share], cache: CacheMode) -> Result<Server, String> {
        Server::with_options(shares, cache, true)
    }

    /// `posix`: Linux guests (exact case, stored owner and mode); false for
    /// Windows guests.
    pub fn with_options(shares: &[Share], cache: CacheMode, posix: bool) -> Result<Server, String> {
        if shares.is_empty() {
            return Err("no shares configured".into());
        }
        let mut fs = VirtioFs::new_aggregate();
        fs.set_cache(cache);
        fs.set_exact_case(posix);
        for s in shares {
            // metadata: Linux owner, group and mode live in NTFS extended
            // attributes (as WSL does), so chmod and chown persist.
            let opts = LxVolumeOptions::from_option_string(match (posix, s.readonly) {
                (true, true) => "metadata;ro",
                (true, false) => "metadata",
                (false, true) => "ro",
                (false, false) => "",
            });
            fs.add_child(&s.name, &s.path, Some(&opts)).map_err(|e| format!("share `{}` ({}): {e}", s.name, s.path))?;
        }
        let trace = std::env::var_os("QORRAL_FS_TRACE").and_then(|p| {
            std::fs::OpenOptions::new().create(true).append(true).open(p).ok().map(std::sync::Mutex::new)
        });
        let paths = shares.iter().map(|s| s.path.clone()).collect();
        Ok(Server { session: Session::new(fs.clone()), fs, paths, trace })
    }

    /// Answers one request. `request` is the device-readable part of the
    /// descriptor chain, `reply` receives the answer. Returns the reply
    /// length (0 for requests without a reply, such as FORGET).
    pub fn handle(&self, request: Vec<u8>, reply: &mut dyn ReplyBuffers) -> usize {
        if let Some(n) = self.lookup_dot(&request, reply) {
            return n;
        }
        if let Some(n) = self.root_statfs(&request, reply) {
            return n;
        }
        let raw = self.trace.as_ref().map(|_| request.iter().take(96).map(|b| format!("{b:02x}")).collect::<String>());
        let request = match Request::new(OwnedReader { data: request, pos: 0 }) {
            Ok(r) => r,
            // Not even a header: there is no request id to answer.
            Err(_) => return 0,
        };
        let (op, node, unique) = (request.opcode(), request.node_id(), request.unique());
        let detail = self.trace.as_ref().map(|_| format!("{:?}", request.operation()));
        let mut sender = Sender { out: reply, written: 0, error: 0 };
        let started = self.trace.as_ref().map(|_| std::time::Instant::now());
        self.session.dispatch(request, &mut sender, None);
        let us = started.map_or(0, |t| t.elapsed().as_micros());
        if let (Some(t), Some(detail)) = (&self.trace, detail) {
            use std::io::Write;
            let mut detail = detail;
            detail.truncate(160);
            if sender.error != 0 {
                detail.push_str(" raw=");
                detail.push_str(raw.as_deref().unwrap_or(""));
            }
            if let Ok(mut f) = t.lock() {
                let _ = writeln!(
                    f,
                    "op={op} node={node} unique={unique} us={us} error={} reply={} {detail}",
                    sender.error, sender.written
                );
            }
        }
        sender.written
    }

    /// `LOOKUP "."` (Windows client): answered here, since the FUSE parser
    /// rejects "." as a name. `None` for every other request.
    fn lookup_dot(&self, request: &[u8], reply: &mut dyn ReplyBuffers) -> Option<usize> {
        use fuse::protocol::{FUSE_LOOKUP, fuse_in_header};
        use zerocopy::FromBytes;
        let (h, body) = fuse_in_header::read_from_prefix(request).ok()?;
        if h.opcode != FUSE_LOOKUP || !(body == b".\0" || body == b".") || !self.session.is_initialized() {
            return None;
        }
        let mut sender = Sender { out: reply, written: 0, error: 0 };
        let _ = match self.fs.lookup_self(h.nodeid) {
            Ok(entry) => sender.send_arg(h.unique, entry),
            Err(e) => sender.send_error(h.unique, e.value()),
        };
        Some(sender.written)
    }

    /// `STATFS` on the synthetic root: the shares' host volume with the most
    /// free space, not zeros (a drive with "0 bytes free" makes Windows
    /// refuse copies). `None` for every other request.
    fn root_statfs(&self, request: &[u8], reply: &mut dyn ReplyBuffers) -> Option<usize> {
        use fuse::protocol::{FUSE_ROOT_ID, FUSE_STATFS, fuse_in_header, fuse_kstatfs};
        use zerocopy::FromBytes;
        let (h, _) = fuse_in_header::read_from_prefix(request).ok()?;
        if h.opcode != FUSE_STATFS || h.nodeid != FUSE_ROOT_ID || !self.session.is_initialized() {
            return None;
        }
        let (total, free) = self.paths.iter().filter_map(|p| disk_space(p)).max_by_key(|&(_, f)| f).unwrap_or((0, 0));
        const BSIZE: u64 = 4096;
        let st = fuse_kstatfs::new(total / BSIZE, free / BSIZE, free / BSIZE, 0, 0, BSIZE as u32, 255, BSIZE as u32);
        let mut sender = Sender { out: reply, written: 0, error: 0 };
        let _ = sender.send_arg(h.unique, fuse::protocol::fuse_statfs_out { st });
        Some(sender.written)
    }

    /// The guest driver went away (device reset): the next request may be
    /// a new FUSE_INIT.
    pub fn reset(&self) {
        self.session.destroy();
    }
}

/// (total, free-to-caller) bytes of the volume holding `path`.
fn disk_space(path: &str) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: NUL-terminated path and valid out pointers.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some((total, avail))
    }
    #[cfg(unix)]
    {
        let c = std::ffi::CString::new(path).ok()?;
        // SAFETY: zeroed POD out-struct, NUL-terminated path.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0;
        let f = st.f_frsize as u64;
        ok.then(|| (st.f_blocks as u64 * f, st.f_bavail as u64 * f))
    }
}

/// Where replies go: the guest's writable buffers.
pub trait ReplyBuffers {
    /// Writes `data` at `offset` into the reply area; returns how much fit.
    fn write_at(&mut self, offset: usize, data: &[u8]) -> usize;
    fn capacity(&self) -> usize;
}

impl ReplyBuffers for Vec<u8> {
    fn write_at(&mut self, offset: usize, data: &[u8]) -> usize {
        if self.len() < offset + data.len() {
            self.resize(offset + data.len(), 0);
        }
        self[offset..offset + data.len()].copy_from_slice(data);
        data.len()
    }
    fn capacity(&self) -> usize {
        usize::MAX
    }
}

struct Sender<'a> {
    out: &'a mut dyn ReplyBuffers,
    written: usize,
    /// The reply header's error field (for the trace).
    error: i32,
}

impl ReplySender for Sender<'_> {
    fn send(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<()> {
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        if total > self.out.capacity() {
            // The guest offered too little room; FUSE replies are all or nothing.
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "reply larger than the guest's buffers"));
        }
        if let Some(h) = bufs.first().filter(|h| h.len() >= 8) {
            self.error = i32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        }
        let mut at = 0;
        for b in bufs {
            at += self.out.write_at(at, b);
        }
        self.written = at;
        Ok(())
    }
}

struct OwnedReader {
    data: Vec<u8>,
    pos: usize,
}

impl Read for OwnedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl RequestReader for OwnedReader {
    fn read_until(&mut self, byte: u8) -> lx::Result<Vec<u8>> {
        let rest = &self.data[self.pos..];
        let n = rest.iter().position(|&b| b == byte).ok_or(lx::Error::EINVAL)?;
        let out = rest[..n].to_vec();
        self.pos += n + 1;
        Ok(out)
    }

    fn remaining_len(&self) -> usize {
        self.data.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuse::protocol::*;
    use zerocopy::{FromBytes, FromZeros, IntoBytes};

    struct Guest {
        server: Server,
        unique: u64,
    }

    impl Guest {
        fn new(shares: &[Share]) -> Guest {
            let mut g = Guest { server: Server::new(shares).unwrap(), unique: 0 };
            let init = fuse_init_in {
                major: FUSE_KERNEL_VERSION,
                minor: FUSE_KERNEL_MINOR_VERSION,
                max_readahead: 128 * 1024,
                flags: 0,
                flags2: 0,
                unused: [0; 11],
            };
            let (h, _) = g.call(FUSE_INIT, 0, &[init.as_bytes()]);
            assert_eq!(h.error, 0, "FUSE_INIT");
            g
        }

        /// Sends one request; returns the reply header and body.
        fn call(&mut self, opcode: u32, nodeid: u64, parts: &[&[u8]]) -> (fuse_out_header, Vec<u8>) {
            self.unique += 1;
            let body: Vec<u8> = parts.concat();
            let header = fuse_in_header {
                len: (size_of::<fuse_in_header>() + body.len()) as u32,
                opcode,
                unique: self.unique,
                nodeid,
                uid: 0,
                gid: 0,
                pid: 1,
                padding: 0,
            };
            let mut msg = header.as_bytes().to_vec();
            msg.extend_from_slice(&body);
            let mut reply = Vec::new();
            let n = self.server.handle(msg, &mut reply);
            assert!(n >= size_of::<fuse_out_header>(), "opcode {opcode} gave no reply");
            let (h, rest) = fuse_out_header::read_from_prefix(&reply[..n]).unwrap();
            assert_eq!(h.unique, self.unique);
            (h, rest.to_vec())
        }

        fn lookup(&mut self, parent: u64, name: &str) -> Result<u64, i32> {
            let (h, body) = self.call(FUSE_LOOKUP, parent, &[name.as_bytes(), &[0]]);
            if h.error != 0 {
                return Err(h.error);
            }
            Ok(fuse_entry_out::read_from_prefix(&body).unwrap().0.nodeid)
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("qorral-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn share(name: &str, path: &std::path::Path, readonly: bool) -> Share {
        Share { name: name.into(), path: path.display().to_string(), readonly }
    }

    #[test]
    fn create_write_read_back_through_fuse() {
        let dir = temp_dir("rw");
        std::fs::write(dir.join("hello.txt"), b"hello from the host").unwrap();
        let mut g = Guest::new(&[share("work", &dir, false)]);
        let work = g.lookup(FUSE_ROOT_ID, "work").expect("share is a child of the root");

        // Read an existing host file.
        let file = g.lookup(work, "hello.txt").unwrap();
        let open = fuse_open_in { flags: 0, unused: 0 };
        let (h, body) = g.call(FUSE_OPEN, file, &[open.as_bytes()]);
        assert_eq!(h.error, 0);
        let fh = fuse_open_out::read_from_prefix(&body).unwrap().0.fh;
        let read = fuse_read_in { fh, offset: 6, size: 64, read_flags: 0, lock_owner: 0, flags: 0, padding: 0 };
        let (h, body) = g.call(FUSE_READ, file, &[read.as_bytes()]);
        assert_eq!(h.error, 0);
        assert_eq!(body, b"from the host");

        // Create a file from the guest; it appears on the host.
        let create =
            fuse_create_in { flags: 0o1 | 0o100 /* O_WRONLY | O_CREAT */, mode: 0o644, umask: 0, padding: 0 };
        let (h, body) = g.call(FUSE_CREATE, work, &[create.as_bytes(), b"new.txt\0"]);
        assert_eq!(h.error, 0, "create");
        let (entry, rest) = fuse_entry_out::read_from_prefix(&body).unwrap();
        let fh = fuse_open_out::read_from_prefix(rest).unwrap().0.fh;
        let data = b"written by the guest";
        let write = fuse_write_in {
            fh,
            offset: 0,
            size: data.len() as u32,
            write_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        let (h, _) = g.call(FUSE_WRITE, entry.nodeid, &[write.as_bytes(), data]);
        assert_eq!(h.error, 0, "write");
        let release = fuse_release_in { fh, flags: 0, release_flags: 0, lock_owner: 0 };
        g.call(FUSE_RELEASE, entry.nodeid, &[release.as_bytes()]);
        assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readonly_shares_refuse_writes_and_unknown_names_fail() {
        let dir = temp_dir("ro");
        let mut g = Guest::new(&[share("docs", &dir, true)]);
        let docs = g.lookup(FUSE_ROOT_ID, "docs").unwrap();
        let create = fuse_create_in { flags: 0o1 | 0o100, mode: 0o644, umask: 0, padding: 0 };
        let (h, _) = g.call(FUSE_CREATE, docs, &[create.as_bytes(), b"nope.txt\0"]);
        assert_eq!(h.error, -lx::EROFS, "a read-only share refuses new files");
        assert!(!dir.join("nope.txt").exists());
        assert!(g.lookup(FUSE_ROOT_ID, "other").is_err(), "only configured shares exist");
        assert!(g.lookup(docs, "..").is_err() || g.lookup(docs, "..") == Ok(FUSE_ROOT_ID));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_dot_answers_with_the_directory_itself() {
        let dir = temp_dir("dot");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut g = Guest::new(&[share("s", &dir, false)]);
        assert_eq!(g.lookup(FUSE_ROOT_ID, "."), Ok(FUSE_ROOT_ID), "the root's own entry");
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        let sub = g.lookup(s, "sub").unwrap();
        assert_eq!(g.lookup(sub, "."), Ok(sub));
        // Two lookups (name and "."): two forgets keep it alive until the second.
        let forget = fuse_forget_in { nlookup: 1 };
        g.server.handle(
            [
                fuse_in_header {
                    len: (size_of::<fuse_in_header>() + size_of::<fuse_forget_in>()) as u32,
                    opcode: FUSE_FORGET,
                    unique: 999,
                    nodeid: sub,
                    uid: 0,
                    gid: 0,
                    pid: 0,
                    padding: 0,
                }
                .as_bytes(),
                forget.as_bytes(),
            ]
            .concat(),
            &mut Vec::new(),
        );
        assert_eq!(g.lookup(sub, "."), Ok(sub), "still known after one forget");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_statfs_reports_the_host_volume() {
        let dir = temp_dir("statfs");
        let mut g = Guest::new(&[share("s", &dir, false)]);
        let (h, body) = g.call(FUSE_STATFS, FUSE_ROOT_ID, &[]);
        assert_eq!(h.error, 0);
        let st = fuse_statfs_out::read_from_prefix(&body).unwrap().0.st;
        assert!(st.blocks > 0 && st.bavail > 0, "real figures, not zeros: {st:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn open_flags(g: &mut Guest, node: u64) -> u32 {
        let (h, body) = g.call(FUSE_OPEN, node, &[fuse_open_in { flags: 0, unused: 0 }.as_bytes()]);
        assert_eq!(h.error, 0);
        fuse_open_out::read_from_prefix(&body).unwrap().0.open_flags
    }

    #[test]
    fn cache_modes_set_open_flags_and_attribute_timeouts() {
        let dir = temp_dir("cache");
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        // strict: direct I/O, 1 ms attributes; both: no flush on close.
        let mut g = Guest::new(&[share("s", &dir, false)]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        let f = g.lookup(s, "f.txt").unwrap();
        let flags = open_flags(&mut g, f);
        assert!(flags & FOPEN_DIRECT_IO != 0 && flags & (1 << 5) != 0, "strict: {flags:#x}");
        // auto: page cache allowed, 1 s attributes.
        let mut g =
            Guest { server: Server::with_cache(&[share("s", &dir, false)], CacheMode::Auto).unwrap(), unique: 0 };
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            flags2: 0,
            unused: [0; 11],
        };
        g.call(FUSE_INIT, 0, &[init.as_bytes()]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        let (_, body) = g.call(FUSE_LOOKUP, s, &[b"f.txt", &[0]]);
        let entry = fuse_entry_out::read_from_prefix(&body).unwrap().0;
        assert_eq!((entry.attr_valid, entry.entry_valid), (1, 1), "attributes and names 1 s");
        let flags = open_flags(&mut g, entry.nodeid);
        assert!(flags & FOPEN_DIRECT_IO == 0 && flags & (1 << 5) != 0, "auto: {flags:#x}");
        assert!(flags & (1 << 1) != 0, "auto keeps the page cache across opens: {flags:#x}");
        assert!(Server::from_json(r#"{"shares":[],"cache":"fast"}"#).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_is_unsupported_for_linux_guests_only() {
        let dir = temp_dir("flush");
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        for (posix, want) in [(true, -38), (false, 0)] {
            let mut g = Guest {
                server: Server::with_options(&[share("s", &dir, false)], CacheMode::Auto, posix).unwrap(),
                unique: 0,
            };
            let init = fuse_init_in {
                major: FUSE_KERNEL_VERSION,
                minor: FUSE_KERNEL_MINOR_VERSION,
                max_readahead: 0,
                flags: 0,
                flags2: 0,
                unused: [0; 11],
            };
            g.call(FUSE_INIT, 0, &[init.as_bytes()]);
            let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
            let f = g.lookup(s, "f.txt").unwrap();
            let (h, body) = g.call(FUSE_OPEN, f, &[fuse_open_in { flags: 0, unused: 0 }.as_bytes()]);
            assert_eq!(h.error, 0);
            let fh = fuse_open_out::read_from_prefix(&body).unwrap().0.fh;
            let flush = fuse_flush_in { fh, unused: 0, padding: 0, lock_owner: 0 };
            let (h, _) = g.call(FUSE_FLUSH, f, &[flush.as_bytes()]);
            // Linux stops sending FLUSH after ENOSYS; Windows reads it as an error.
            assert_eq!(h.error, want, "posix={posix}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn create_file(g: &mut Guest, dir: u64, name: &str) -> i32 {
        let create = fuse_create_in { flags: 0o1 | 0o100, mode: 0o644, umask: 0, padding: 0 };
        let (h, _) = g.call(FUSE_CREATE, dir, &[create.as_bytes(), name.as_bytes(), &[0]]);
        h.error
    }

    #[test]
    fn names_differing_only_in_case_never_overwrite_each_other() {
        let dir = temp_dir("case");
        let mut g = Guest::new(&[share("s", &dir, false)]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        assert_eq!(create_file(&mut g, s, "Readme"), 0);
        assert_eq!(g.lookup(s, "readme"), Err(-lx::ENOENT), "a different-case name is not found");
        assert_eq!(create_file(&mut g, s, "readme"), -lx::EEXIST, "and cannot be created over it");
        let mkdir = fuse_mkdir_in { mode: 0o755, umask: 0 };
        let (h, _) = g.call(FUSE_MKDIR, s, &[mkdir.as_bytes(), b"README", &[0]]);
        assert_eq!(h.error, -lx::EEXIST);
        // Renaming another file onto a case twin is refused; a case-only
        // rename of the same file is fine.
        assert_eq!(create_file(&mut g, s, "other"), 0);
        let rn = fuse_rename_in { newdir: s };
        let (h, _) = g.call(FUSE_RENAME, s, &[rn.as_bytes(), b"other\0README\0"]);
        assert_eq!(h.error, -lx::EEXIST);
        let (h, _) = g.call(FUSE_RENAME, s, &[rn.as_bytes(), b"Readme\0README\0"]);
        assert_eq!(h.error, 0, "case-only rename of the same file");
        let names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"README".to_string()) && names.contains(&"other".to_string()), "{names:?}");
        assert!(g.lookup(s, "README").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_guests_get_case_insensitive_names() {
        let dir = temp_dir("wincase");
        std::fs::write(dir.join("Readme.txt"), b"x").unwrap();
        let mut g = Guest {
            server: Server::with_options(&[share("s", &dir, false)], CacheMode::Auto, false).unwrap(),
            unique: 0,
        };
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            flags2: 0,
            unused: [0; 11],
        };
        g.call(FUSE_INIT, 0, &[init.as_bytes()]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        assert!(g.lookup(s, "README.TXT").is_ok(), "any case finds the file, as on Windows");
        assert!(
            Server::from_json(&format!(
                r#"{{"shares":[{{"name":"a","path":{:?}}}],"posix":false}}"#,
                dir.display().to_string()
            ))
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chmod_and_chown_persist() {
        let dir = temp_dir("meta");
        let mut g = Guest::new(&[share("s", &dir, false)]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        assert_eq!(create_file(&mut g, s, "script.sh"), 0);
        let f = g.lookup(s, "script.sh").unwrap();
        let set = |valid, mode, uid| fuse_setattr_in {
            valid,
            padding: 0,
            fh: 0,
            size: 0,
            lock_owner: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            atimensec: 0,
            mtimensec: 0,
            ctimensec: 0,
            mode,
            unused4: 0,
            uid,
            gid: uid,
            unused5: 0,
        };
        let (h, _) = g.call(FUSE_SETATTR, f, &[set(FATTR_MODE, 0o100750, 0).as_bytes()]);
        assert_eq!(h.error, 0, "chmod");
        let (h, _) = g.call(FUSE_SETATTR, f, &[set(FATTR_UID | FATTR_GID, 0, 1000).as_bytes()]);
        assert_eq!(h.error, 0, "chown");
        let (h, body) = g.call(FUSE_GETATTR, f, &[fuse_getattr_in { getattr_flags: 0, dummy: 0, fh: 0 }.as_bytes()]);
        assert_eq!(h.error, 0);
        let attr = fuse_attr_out::read_from_prefix(&body).unwrap().0.attr;
        assert_eq!((attr.mode & 0o7777, attr.uid, attr.gid), (0o750, 1000, 1000), "mode and owner persist");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writeback_cache_in_auto_mode_reads_write_only_files() {
        let dir = temp_dir("wb");
        std::fs::write(
            dir.join("log.txt"),
            b"one
",
        )
        .unwrap();
        let mut g =
            Guest { server: Server::with_cache(&[share("s", &dir, false)], CacheMode::Auto).unwrap(), unique: 0 };
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 1 << 20,
            flags: FUSE_WRITEBACK_CACHE | FUSE_INIT_EXT,
            flags2: 0,
            unused: [0; 11],
        };
        let (h, body) = g.call(FUSE_INIT, 0, &[init.as_bytes()]);
        assert_eq!(h.error, 0);
        let out = fuse_init_out::read_from_prefix(&body).unwrap().0;
        assert!(out.flags & FUSE_WRITEBACK_CACHE != 0, "auto mode takes the writeback cache");
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        let f = g.lookup(s, "log.txt").unwrap();
        // O_WRONLY | O_APPEND: the kernel will still read pages through it.
        let (h, body) = g.call(FUSE_OPEN, f, &[fuse_open_in { flags: 0o1 | 0o2000, unused: 0 }.as_bytes()]);
        assert_eq!(h.error, 0);
        let fh = fuse_open_out::read_from_prefix(&body).unwrap().0.fh;
        let rd = fuse_read_in { fh, offset: 0, size: 64, read_flags: 0, lock_owner: 0, flags: 0, padding: 0 };
        let (h, body) = g.call(FUSE_READ, f, &[rd.as_bytes()]);
        assert_eq!(
            (h.error, body.as_slice()),
            (
                0,
                b"one
"
                .as_slice()
            ),
            "readable for the page cache"
        );
        // Kernel-positioned write at the end (no host-side O_APPEND).
        let data = b"two
";
        let w = fuse_write_in { fh, offset: 4, size: 4, write_flags: 0, lock_owner: 0, flags: 0, padding: 0 };
        let (h, _) = g.call(FUSE_WRITE, f, &[w.as_bytes(), data]);
        assert_eq!(h.error, 0);
        assert_eq!(
            std::fs::read(dir.join("log.txt")).unwrap(),
            b"one
two
"
        );
        // Strict mode never takes it.
        let mut g2 = Guest { server: Server::new(&[share("s", &dir, false)]).unwrap(), unique: 0 };
        let (_, body) = g2.call(FUSE_INIT, 0, &[init.as_bytes()]);
        assert!(fuse_init_out::read_from_prefix(&body).unwrap().0.flags & FUSE_WRITEBACK_CACHE == 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn getattr(g: &mut Guest, node: u64) -> i32 {
        let (h, _) = g.call(FUSE_GETATTR, node, &[fuse_getattr_in { getattr_flags: 0, dummy: 0, fh: 0 }.as_bytes()]);
        h.error
    }

    #[test]
    fn renames_move_cached_nodes_along() {
        let dir = temp_dir("mv");
        std::fs::create_dir_all(dir.join("d/sub")).unwrap();
        std::fs::write(dir.join("d/sub/f.txt"), b"x").unwrap();
        std::fs::write(dir.join("victim"), b"old").unwrap();
        std::fs::write(dir.join("mover"), b"new").unwrap();
        let mut g =
            Guest { server: Server::with_cache(&[share("s", &dir, false)], CacheMode::Auto).unwrap(), unique: 0 };
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 0,
            flags: 0,
            flags2: 0,
            unused: [0; 11],
        };
        g.call(FUSE_INIT, 0, &[init.as_bytes()]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        let d = g.lookup(s, "d").unwrap();
        let sub = g.lookup(d, "sub").unwrap();
        let f = g.lookup(sub, "f.txt").unwrap();
        // The guest keeps these node ids cached; rename the top directory.
        let rn = fuse_rename_in { newdir: s };
        let (h, _) = g.call(FUSE_RENAME, s, &[rn.as_bytes(), b"d\0e\0"]);
        assert_eq!(h.error, 0);
        assert_eq!(getattr(&mut g, f), 0, "a node below the renamed directory still resolves");
        assert_eq!(getattr(&mut g, sub), 0);
        // Renaming onto an existing file: the replaced node no longer resolves,
        // and must not reach the file that took its place.
        let victim = g.lookup(s, "victim").unwrap();
        let mover = g.lookup(s, "mover").unwrap();
        let (h, _) = g.call(FUSE_RENAME, s, &[rn.as_bytes(), b"mover\0victim\0"]);
        assert_eq!(h.error, 0);
        assert_eq!(getattr(&mut g, mover), 0, "the moved file resolves under its new name");
        assert_eq!(getattr(&mut g, victim), -lx::ENOENT, "the replaced file is gone");
        assert_eq!(std::fs::read(dir.join("victim")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_setattr_after_rename_uses_the_open_handle() {
        let dir = temp_dir("stale");
        let mut g = Guest::new(&[share("s", &dir, false)]);
        let s = g.lookup(FUSE_ROOT_ID, "s").unwrap();
        assert_eq!(create_file(&mut g, s, "a"), 0);
        let f = g.lookup(s, "a").unwrap();
        let (h, body) = g.call(FUSE_OPEN, f, &[fuse_open_in { flags: 0o2, unused: 0 }.as_bytes()]);
        assert_eq!(h.error, 0);
        let _fh = fuse_open_out::read_from_prefix(&body).unwrap().0.fh;
        let rn = fuse_rename_in { newdir: s };
        let (h, _) = g.call(FUSE_RENAME, s, &[rn.as_bytes(), b"a\0b\0"]);
        assert_eq!(h.error, 0);
        // Node-based times update (no FATTR_FH), as the writeback cache sends it.
        let mut set = fuse_setattr_in::new_zeroed();
        set.valid = FATTR_MTIME | FATTR_CTIME;
        set.mtime = 1_000_000_000;
        set.ctime = 1_000_000_000;
        let (h, _) = g.call(FUSE_SETATTR, f, &[set.as_bytes()]);
        assert_eq!(h.error, 0, "applied through the open handle");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_and_short_buffers_never_panic() {
        let dir = temp_dir("junk");
        let g = Guest::new(&[share("s", &dir, false)]);
        assert_eq!(g.server.handle(vec![], &mut Vec::new()), 0);
        assert_eq!(g.server.handle(vec![0xFF; 7], &mut Vec::new()), 0);
        // A well-formed header with an unknown opcode gets an error reply.
        let header = fuse_in_header {
            len: size_of::<fuse_in_header>() as u32,
            opcode: 9999,
            unique: 77,
            nodeid: 1,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let mut reply = Vec::new();
        let n = g.server.handle(header.as_bytes().to_vec(), &mut reply);
        let (h, _) = fuse_out_header::read_from_prefix(&reply[..n]).unwrap();
        assert!(h.error < 0 && h.unique == 77);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_json_needs_shares_that_exist() {
        let dir = temp_dir("cfg");
        let json = format!(r#"{{"shares":[{{"name":"a","path":{:?}}}]}}"#, dir.display().to_string());
        assert!(Server::from_json(&json).is_ok());
        assert!(Server::from_json(r#"{"shares":[]}"#).is_err());
        assert!(Server::from_json(r#"{"shares":[{"name":"x","path":"Z:\\does\\not\\exist"}]}"#).is_err());
        assert!(Server::from_json("{oops").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
