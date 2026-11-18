//! Windows plumbing: AF_UNIX socket pairs handed to QEMU, and QEMU's shared memory mappings.
//!
//! QEMU only shares memory or textures with a listener on an AF_UNIX connection, because it
//! reads the peer's process id from the socket to duplicate handles into it.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, INVALID_SOCKET, SD_BOTH, SOCK_STREAM, SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR, WSADATA,
    WSADuplicateSocketW, WSAPROTOCOL_INFOW, WSAStartup, accept, bind, closesocket, connect, listen, recv, send,
    shutdown, socket,
};
use windows_sys::Win32::System::Memory::{FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

fn startup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        // SAFETY: WSAStartup only writes into `data`.
        unsafe { WSAStartup(0x0202, &mut data) };
    });
}

/// An owned socket. It can be closed while another thread is blocked reading it: on Windows
/// AF_UNIX sockets, `shutdown` does not wake a blocked `recv`, closing does.
#[derive(Debug)]
pub struct Sock(AtomicUsize);

impl Drop for Sock {
    fn drop(&mut self) {
        self.close();
    }
}

impl Sock {
    fn new() -> io::Result<Sock> {
        startup();
        // SAFETY: plain socket creation.
        let s = unsafe { socket(i32::from(AF_UNIX), SOCK_STREAM, 0) };
        if s == INVALID_SOCKET { Err(io::Error::last_os_error()) } else { Ok(Sock(AtomicUsize::new(s))) }
    }

    fn raw(&self) -> SOCKET {
        self.0.load(Ordering::Acquire)
    }

    /// Closes the socket once; readers and writers on other threads get an error from then on.
    pub fn close(&self) {
        let s = self.0.swap(INVALID_SOCKET, Ordering::AcqRel);
        if s != INVALID_SOCKET {
            // SAFETY: the socket was owned and is closed exactly once (the swap above).
            unsafe {
                shutdown(s, SD_BOTH);
                closesocket(s);
            }
        }
    }

    /// The `WSAPROTOCOL_INFOW` bytes with which process `pid` can import this socket
    /// (`WSASocketW(FROM_PROTOCOL_INFO…)`; QMP `get-win32-socket` takes them in base64).
    pub fn duplicate_for(&self, pid: u32) -> io::Result<Vec<u8>> {
        let mut info: WSAPROTOCOL_INFOW = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid out pointer for the call.
        if unsafe { WSADuplicateSocketW(self.raw(), pid, &mut info) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a plain C struct read as bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts((&raw const info).cast::<u8>(), std::mem::size_of::<WSAPROTOCOL_INFOW>())
        };
        Ok(bytes.to_vec())
    }
}

/// A connected AF_UNIX socket pair, through a temporary socket file that is removed at once.
pub fn pair() -> io::Result<(Sock, Sock)> {
    static N: AtomicU32 = AtomicU32::new(0);
    let path =
        std::env::temp_dir().join(format!("qorral-{}-{}.sock", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_file(&path);
    let text = path.to_str().filter(|p| p.is_ascii() && p.len() < 108).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("socket path not usable: {}", path.display()))
    })?;
    let mut addr: SOCKADDR_UN = unsafe { std::mem::zeroed() };
    addr.sun_family = AF_UNIX;
    for (d, s) in addr.sun_path.iter_mut().zip(text.bytes()) {
        *d = s as i8;
    }
    let len = std::mem::size_of::<SOCKADDR_UN>() as i32;
    let addr_ptr = (&raw const addr).cast::<SOCKADDR>();

    let listener = Sock::new()?;
    // SAFETY: valid address of the given length.
    if unsafe { bind(listener.raw(), addr_ptr, len) } == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        // SAFETY: owned sockets, valid address.
        if unsafe { listen(listener.raw(), 1) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        let client = Sock::new()?;
        if unsafe { connect(client.raw(), addr_ptr, len) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        let server = unsafe { accept(listener.raw(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if server == INVALID_SOCKET {
            return Err(io::Error::last_os_error());
        }
        Ok((Sock(AtomicUsize::new(server)), client))
    })();
    drop(listener);
    let _ = std::fs::remove_file(&path);
    result
}

/// A socket shared by a reading and a writing thread.
#[derive(Clone, Debug)]
pub struct Stream(pub Arc<Sock>);

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `buf` is valid for `n` bytes.
        let r = unsafe { recv(self.0.raw(), buf.as_mut_ptr(), n, 0) };
        if r == SOCKET_ERROR { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `buf` is valid for `n` bytes.
        let r = unsafe { send(self.0.raw(), buf.as_ptr(), n, 0) };
        if r == SOCKET_ERROR { Err(io::Error::last_os_error()) } else { Ok(r as usize) }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A read-only view of a shared file mapping QEMU duplicated into this process. Owns the handle.
pub struct SharedMap {
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    /// Where the requested bytes start in the view (offsets must be granularity-aligned).
    skip: usize,
    len: usize,
    handle: HANDLE,
}

// SAFETY: the view is read-only memory that stays valid until drop.
unsafe impl Send for SharedMap {}
unsafe impl Sync for SharedMap {}

impl SharedMap {
    /// Maps `len` bytes at `offset` of the file mapping `handle`, taking ownership of the handle
    /// (it is closed on drop, and also when mapping fails).
    pub fn open(handle: u64, offset: u32, len: usize) -> io::Result<SharedMap> {
        let handle = handle as usize as HANDLE;
        let mut si: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: only writes `si`.
        unsafe { GetSystemInfo(&mut si) };
        let gran = si.dwAllocationGranularity.max(1);
        let base = offset - offset % gran;
        let skip = (offset - base) as usize;
        // SAFETY: mapping a handle this process owns; failure is checked.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, base, skip + len) };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            // SAFETY: the handle is ours.
            unsafe { CloseHandle(handle) };
            return Err(e);
        }
        Ok(SharedMap { view, skip, len, handle })
    }

    /// The mapped bytes. QEMU keeps writing them while the guest draws: read rectangles after
    /// their `UpdateMap`, and expect tearing only within a frame.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the view covers `skip + len` readable bytes until drop.
        unsafe { std::slice::from_raw_parts(self.view.Value.cast::<u8>().add(self.skip), self.len) }
    }
}

impl Drop for SharedMap {
    fn drop(&mut self) {
        // SAFETY: unmapping our view and closing our handle once.
        unsafe {
            UnmapViewOfFile(self.view);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_pair_carries_bytes_both_ways() {
        let (a, b) = pair().unwrap();
        let (mut a, mut b) = (Stream(Arc::new(a)), Stream(Arc::new(b)));
        a.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        b.write_all(b"pong").unwrap();
        a.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"pong");
        assert_eq!(a.0.duplicate_for(std::process::id()).unwrap().len(), std::mem::size_of::<WSAPROTOCOL_INFOW>());
    }

    #[test]
    fn shutdown_wakes_a_blocked_reader() {
        let (a, _b) = pair().unwrap();
        let a = Stream(Arc::new(a));
        let mut r = a.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4];
            let _ = tx.send(r.read(&mut buf).map_err(|e| e.kind()));
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        a.0.close();
        let got = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert!(got.is_ok(), "the reader is still blocked after shutdown");
    }
}
