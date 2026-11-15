//! C interface for QEMU's in-process virtio-fs device.
//!
//! ```c
//! typedef struct { void *base; size_t len; } QfsIovec;
//! uint32_t qfs_abi_version(void);                       // 1
//! void    *qfs_create(const char *config_json, char *err, size_t err_len);
//! size_t   qfs_handle(void *fs, const QfsIovec *in, size_t n_in,
//!                     const QfsIovec *out, size_t n_out);  // reply length
//! void     qfs_reset(void *fs);                          // device reset
//! void     qfs_destroy(void *fs);
//! ```
//!
//! `qfs_handle` is thread-safe: QEMU may call it from several request-queue
//! threads at once. A panic inside never crosses into QEMU; the request
//! then gets no reply (the guest sees an I/O error) and the server stays up.

use crate::server::{ReplyBuffers, Server};
use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

#[repr(C)]
pub struct QfsIovec {
    pub base: *mut u8,
    pub len: usize,
}

pub const ABI_VERSION: u32 = 1;

#[unsafe(no_mangle)]
pub extern "C" fn qfs_abi_version() -> u32 {
    ABI_VERSION
}

/// Creates a server from the JSON share list; NULL on error, with the
/// message (NUL-terminated, truncated) in `err`.
///
/// # Safety
/// `config` is a NUL-terminated string; `err` points to `err_len` writable bytes or is NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qfs_create(config: *const c_char, err: *mut c_char, err_len: usize) -> *mut Server {
    let result = catch_unwind(|| {
        if config.is_null() {
            return Err("no configuration".to_string());
        }
        // SAFETY: the caller passes a NUL-terminated string.
        let text = unsafe { CStr::from_ptr(config) }.to_string_lossy();
        Server::from_json(&text)
    })
    .unwrap_or_else(|_| Err("internal error while creating the file server".into()));
    match result {
        Ok(s) => Box::into_raw(Box::new(s)),
        Err(msg) => {
            if !err.is_null() && err_len > 0 {
                let n = msg.len().min(err_len - 1);
                // SAFETY: the caller guarantees `err_len` writable bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(msg.as_ptr(), err.cast::<u8>(), n);
                    *err.add(n) = 0;
                }
            }
            std::ptr::null_mut()
        }
    }
}

/// The guest's writable descriptors. Writes go through raw copies, never
/// Rust references, since the guest may touch its memory concurrently.
struct IovWriter<'a> {
    iov: &'a [QfsIovec],
}

impl ReplyBuffers for IovWriter<'_> {
    fn write_at(&mut self, mut offset: usize, data: &[u8]) -> usize {
        let mut done = 0;
        for v in self.iov {
            if done == data.len() {
                break;
            }
            if offset >= v.len {
                offset -= v.len;
                continue;
            }
            let n = (v.len - offset).min(data.len() - done);
            // SAFETY: QEMU maps each descriptor as `len` writable bytes for
            // the duration of the call; `offset + n <= len`.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), v.base.add(offset), n) };
            done += n;
            offset = 0;
        }
        done
    }

    fn capacity(&self) -> usize {
        self.iov.iter().map(|v| v.len).sum()
    }
}

/// Answers one request; returns the number of reply bytes written.
///
/// # Safety
/// `fs` comes from `qfs_create`; `input`/`output` point to `n_in`/`n_out`
/// descriptors whose buffers stay mapped for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qfs_handle(
    fs: *const Server,
    input: *const QfsIovec,
    n_in: usize,
    output: *const QfsIovec,
    n_out: usize,
) -> usize {
    if fs.is_null() {
        return 0;
    }
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: per the contract above.
        let (fs, ins, outs) = unsafe { (&*fs, slice_or_empty(input, n_in), slice_or_empty(output, n_out)) };
        // Copy the request out of guest memory once, then parse the copy.
        let mut request = Vec::with_capacity(ins.iter().map(|v| v.len).sum());
        for v in ins {
            let start = request.len();
            request.resize(start + v.len, 0);
            // SAFETY: each input descriptor is `len` readable bytes.
            unsafe { std::ptr::copy_nonoverlapping(v.base, request[start..].as_mut_ptr(), v.len) };
        }
        fs.handle(request, &mut IovWriter { iov: outs })
    }))
    .unwrap_or(0)
}

unsafe fn slice_or_empty<'a>(p: *const QfsIovec, n: usize) -> &'a [QfsIovec] {
    if p.is_null() || n == 0 {
        &[]
    } else {
        // SAFETY: the caller guarantees `n` elements at `p`.
        unsafe { std::slice::from_raw_parts(p, n) }
    }
}

/// # Safety
/// `fs` comes from `qfs_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qfs_reset(fs: *const Server) {
    if !fs.is_null() {
        // SAFETY: valid server pointer per the contract.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe { &*fs }.reset()));
    }
}

/// # Safety
/// `fs` comes from `qfs_create` and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qfs_destroy(fs: *mut Server) {
    if !fs.is_null() {
        // SAFETY: ownership returns from the caller.
        drop(unsafe { Box::from_raw(fs) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_reports_errors_and_reply_spans_descriptors() {
        let mut err = [0 as c_char; 64];
        let cfg = c"{\"shares\":[]}";
        // SAFETY: valid string and buffer.
        let s = unsafe { qfs_create(cfg.as_ptr(), err.as_mut_ptr(), err.len()) };
        assert!(s.is_null());
        // SAFETY: NUL-terminated by qfs_create.
        let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy();
        assert!(msg.contains("no shares"), "{msg}");

        let (mut a, mut b) = ([0u8; 3], [0u8; 5]);
        let iov = [QfsIovec { base: a.as_mut_ptr(), len: 3 }, QfsIovec { base: b.as_mut_ptr(), len: 5 }];
        let mut w = IovWriter { iov: &iov };
        assert_eq!(w.capacity(), 8);
        assert_eq!(w.write_at(0, b"abcd"), 4);
        assert_eq!(w.write_at(4, b"efgh"), 4);
        assert_eq!((&a, &b), (b"abc", b"defgh"));
        // SAFETY: null server is rejected.
        assert_eq!(unsafe { qfs_handle(std::ptr::null(), std::ptr::null(), 0, std::ptr::null(), 0) }, 0);
    }
}
