//! Host-only cost of the file server: the same FUSE operations the guest
//! benchmark does, without a VM, to separate file-system cost from the
//! virtio transport. `cargo run --release --example bench -- <folder>`

use fuse::protocol::*;
use qorral_fs::{Server, Share};
use std::time::Instant;
use zerocopy::{FromBytes, IntoBytes};

struct Client {
    server: Server,
    unique: u64,
}

impl Client {
    fn call(&mut self, opcode: u32, nodeid: u64, parts: &[&[u8]]) -> (i32, Vec<u8>) {
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
        let (h, rest) = fuse_out_header::read_from_prefix(&reply[..n]).expect("reply header");
        (h.error, rest.to_vec())
    }

    fn lookup(&mut self, parent: u64, name: &str) -> u64 {
        let (e, body) = self.call(FUSE_LOOKUP, parent, &[name.as_bytes(), &[0]]);
        assert_eq!(e, 0, "lookup {name}");
        fuse_entry_out::read_from_prefix(&body).unwrap().0.nodeid
    }
}

fn main() {
    let root = std::env::args().nth(1).expect("folder to use");
    let dir = std::path::Path::new(&root).join("host-bench");
    let _ = std::fs::create_dir_all(&dir);
    let server = Server::new(&[Share { name: "b".into(), path: dir.display().to_string(), readonly: false }]).unwrap();
    let mut c = Client { server, unique: 0 };
    let init = fuse_init_in {
        major: FUSE_KERNEL_VERSION,
        minor: FUSE_KERNEL_MINOR_VERSION,
        max_readahead: 1 << 20,
        flags: 0,
        flags2: 0,
        unused: [0; 11],
    };
    assert_eq!(c.call(FUSE_INIT, 0, &[init.as_bytes()]).0, 0);
    let b = c.lookup(FUSE_ROOT_ID, "b");

    let t = Instant::now();
    let mut nodes = Vec::new();
    for i in 0..1000 {
        let create = fuse_create_in { flags: 0o1 | 0o100 | 0o1000, mode: 0o644, umask: 0, padding: 0 };
        let name = format!("f{i}.txt\0");
        let (e, body) = c.call(FUSE_CREATE, b, &[create.as_bytes(), name.as_bytes()]);
        assert_eq!(e, 0, "create");
        let (entry, rest) = fuse_entry_out::read_from_prefix(&body).unwrap();
        let fh = fuse_open_out::read_from_prefix(rest).unwrap().0.fh;
        let data = format!("file {i}\n");
        let w = fuse_write_in {
            fh,
            offset: 0,
            size: data.len() as u32,
            write_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        c.call(FUSE_WRITE, entry.nodeid, &[w.as_bytes(), data.as_bytes()]);
        let f = fuse_flush_in { fh, unused: 0, padding: 0, lock_owner: 0 };
        c.call(FUSE_FLUSH, entry.nodeid, &[f.as_bytes()]);
        let r = fuse_release_in { fh, flags: 0, release_flags: 0, lock_owner: 0 };
        c.call(FUSE_RELEASE, entry.nodeid, &[r.as_bytes()]);
        nodes.push(entry.nodeid);
    }
    let create_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    for i in 0..1000 {
        let n = c.lookup(b, &format!("f{i}.txt"));
        let g = fuse_getattr_in { getattr_flags: 0, dummy: 0, fh: 0 };
        c.call(FUSE_GETATTR, n, &[g.as_bytes()]);
    }
    let stat_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    for &n in &nodes {
        let (e, body) = c.call(FUSE_OPEN, n, &[fuse_open_in { flags: 0, unused: 0 }.as_bytes()]);
        assert_eq!(e, 0);
        let fh = fuse_open_out::read_from_prefix(&body).unwrap().0.fh;
        let rd = fuse_read_in { fh, offset: 0, size: 4096, read_flags: 0, lock_owner: 0, flags: 0, padding: 0 };
        c.call(FUSE_READ, n, &[rd.as_bytes()]);
        let r = fuse_release_in { fh, flags: 0, release_flags: 0, lock_owner: 0 };
        c.call(FUSE_RELEASE, n, &[r.as_bytes()]);
    }
    let read_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Same files again: a slow first pass and a fast second one point at
    // on-access scanning of freshly written files, not at the server.
    let t = Instant::now();
    for &n in &nodes {
        let (e, body) = c.call(FUSE_OPEN, n, &[fuse_open_in { flags: 0, unused: 0 }.as_bytes()]);
        assert_eq!(e, 0);
        let fh = fuse_open_out::read_from_prefix(&body).unwrap().0.fh;
        let r = fuse_release_in { fh, flags: 0, release_flags: 0, lock_owner: 0 };
        c.call(FUSE_RELEASE, n, &[r.as_bytes()]);
    }
    println!("second open pass: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    println!(
        "host only, 1000 files: create+write {create_ms:.0} ms ({:.0} us/file), lookup+getattr {stat_ms:.0} ms, open+read {read_ms:.0} ms; {} requests",
        create_ms, c.unique
    );
    for i in 0..1000 {
        let _ = std::fs::remove_file(dir.join(format!("f{i}.txt")));
    }
    let _ = std::fs::remove_dir(&dir);
}
