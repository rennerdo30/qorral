//! Does the file server scale across threads? Each thread sends GETXATTR
//! (a miss) for its own file, as the guest's parallel benchmark does.
//! `cargo run --release --example par -- <folder> [requests per thread]`

use fuse::protocol::*;
use qorral_fs::{Server, Share};
use std::time::Instant;
use zerocopy::{FromBytes, FromZeros, IntoBytes};

fn call(server: &Server, opcode: u32, nodeid: u64, parts: &[&[u8]]) -> (i32, Vec<u8>) {
    let body: Vec<u8> = parts.concat();
    let header = fuse_in_header {
        len: (size_of::<fuse_in_header>() + body.len()) as u32,
        opcode,
        unique: 1,
        nodeid,
        uid: 0,
        gid: 0,
        pid: 1,
        padding: 0,
    };
    let mut msg = header.as_bytes().to_vec();
    msg.extend_from_slice(&body);
    let mut reply = Vec::new();
    let n = server.handle(msg, &mut reply);
    let (h, rest) = fuse_out_header::read_from_prefix(&reply[..n]).expect("reply header");
    (h.error, rest.to_vec())
}

fn lookup(server: &Server, parent: u64, name: &str) -> u64 {
    let (e, body) = call(server, FUSE_LOOKUP, parent, &[name.as_bytes(), &[0]]);
    assert_eq!(e, 0, "lookup {name}");
    fuse_entry_out::read_from_prefix(&body).unwrap().0.nodeid
}

fn main() {
    let root = std::env::args().nth(1).expect("folder to use");
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let dir = std::path::Path::new(&root).join("host-par");
    let _ = std::fs::create_dir_all(&dir);
    for i in 0..8 {
        let _ = std::fs::write(dir.join(format!("p{i}")), b"x");
    }
    let server = Server::new(&[Share { name: "b".into(), path: dir.display().to_string(), readonly: false }]).unwrap();
    let init = fuse_init_in {
        major: FUSE_KERNEL_VERSION,
        minor: FUSE_KERNEL_MINOR_VERSION,
        max_readahead: 1 << 20,
        flags: 0,
        flags2: 0,
        unused: [0; 11],
    };
    assert_eq!(call(&server, FUSE_INIT, 0, &[init.as_bytes()]).0, 0);
    let b = lookup(&server, FUSE_ROOT_ID, "b");
    let nodes: Vec<u64> = (0..8).map(|i| lookup(&server, b, &format!("p{i}"))).collect();
    let xin = fuse_getxattr_in { size: 256, padding: 0 };
    let xin: &[u8] = xin.as_bytes();
    let name: &[u8] = b"user.qorral-rt-none\0";
    if std::env::var_os("GAP").is_some() {
        // One request, then a sleep (the core idles, as between guest
        // requests); time only the request.
        for (label, opcode) in [("getxattr", FUSE_GETXATTR), ("statfs", FUSE_STATFS)] {
            let mut t: Vec<f64> = Vec::new();
            for _ in 0..500 {
                std::thread::sleep(std::time::Duration::from_micros(200));
                let t0 = Instant::now();
                match opcode {
                    FUSE_GETXATTR => call(&server, opcode, nodes[0], &[xin, name]),
                    _ => call(&server, opcode, nodes[0], &[]),
                };
                t.push(t0.elapsed().as_secs_f64() * 1e6);
            }
            t.sort_by(|a, b| a.total_cmp(b));
            println!("{label:9} after idle: p10 {:5.1} p50 {:5.1} p90 {:5.1} us", t[50], t[250], t[450]);
        }
        return;
    }
    for (label, opcode) in [("getxattr", FUSE_GETXATTR), ("getattr", FUSE_GETATTR), ("statfs", FUSE_STATFS)] {
        for threads in [1usize, 2, 4] {
            let t0 = Instant::now();
            std::thread::scope(|s| {
                for &node in &nodes[..threads] {
                    let server = &server;
                    s.spawn(move || {
                        for _ in 0..n {
                            match opcode {
                                FUSE_GETXATTR => call(server, opcode, node, &[xin, name]),
                                FUSE_GETATTR => call(server, opcode, node, &[fuse_getattr_in::new_zeroed().as_bytes()]),
                                _ => call(server, opcode, node, &[]),
                            };
                        }
                    });
                }
            });
            let dt = t0.elapsed().as_secs_f64();
            println!(
                "{label:9} {threads} threads: {:7.0} requests/s, {:5.1} us per request per thread",
                (threads * n) as f64 / dt,
                dt / n as f64 * 1e6
            );
        }
    }
}
