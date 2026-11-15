//! One file server per virtio-fs device: host folders as named shares
//! under one root, answering FUSE requests.

use crate::vfs::VirtioFs;
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
}

pub struct Server {
    session: Session,
}

impl Server {
    /// `{"shares":[{"name":"docs","path":"C:\\Users\\me\\Documents","readonly":true}]}`
    pub fn from_json(config: &str) -> Result<Server, String> {
        let c: Config = serde_json::from_str(config).map_err(|e| format!("share config: {e}"))?;
        Server::new(&c.shares)
    }

    /// Every share is a child of one synthetic, read-only root, so a guest
    /// mounts the device once and sees all shares by name.
    pub fn new(shares: &[Share]) -> Result<Server, String> {
        if shares.is_empty() {
            return Err("no shares configured".into());
        }
        let fs = VirtioFs::new_aggregate();
        for s in shares {
            let opts = LxVolumeOptions::from_option_string(if s.readonly { "ro" } else { "" });
            fs.add_child(&s.name, &s.path, Some(&opts)).map_err(|e| format!("share `{}` ({}): {e}", s.name, s.path))?;
        }
        Ok(Server { session: Session::new(fs) })
    }

    /// Answers one request. `request` is the device-readable part of the
    /// descriptor chain, `reply` receives the answer. Returns the reply
    /// length (0 for requests without a reply, such as FORGET).
    pub fn handle(&self, request: Vec<u8>, reply: &mut dyn ReplyBuffers) -> usize {
        let request = match Request::new(OwnedReader { data: request, pos: 0 }) {
            Ok(r) => r,
            // Not even a header: there is no request id to answer.
            Err(_) => return 0,
        };
        let mut sender = Sender { out: reply, written: 0 };
        self.session.dispatch(request, &mut sender, None);
        sender.written
    }

    /// The guest driver went away (device reset): the next request may be
    /// a new FUSE_INIT.
    pub fn reset(&self) {
        self.session.destroy();
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
}

impl ReplySender for Sender<'_> {
    fn send(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<()> {
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        if total > self.out.capacity() {
            // The guest offered too little room; FUSE replies are all or nothing.
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "reply larger than the guest's buffers"));
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
    use zerocopy::{FromBytes, IntoBytes};

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
