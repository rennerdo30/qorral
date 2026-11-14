//! Single instance: one Qorral window per user. A second launch hands over
//! to the first (which comes to the front) and exits.
//!
//! The first instance holds an exclusive lock on `gui.lock` for its whole
//! life (the OS drops it with the process, so a crash leaves nothing stale)
//! and listens on a loopback port written to `gui.port`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

pub enum Instance {
    /// We are the first: keep `_lock` alive; `listener` receives hand-overs.
    First { _lock: File, listener: TcpListener },
    /// Another window runs and was asked to come to the front.
    Handed,
}

/// Takes the lock in `dir`, or hands over to the running instance.
/// Errors (an unwritable folder) fall back to running as the first.
pub fn claim(dir: &Path) -> std::io::Result<Instance> {
    std::fs::create_dir_all(dir)?;
    let lock = File::options().create(true).truncate(false).write(true).open(dir.join("gui.lock"))?;
    match lock.try_lock() {
        Ok(()) => {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            std::fs::write(dir.join("gui.port"), listener.local_addr()?.port().to_string())?;
            Ok(Instance::First { _lock: lock, listener })
        }
        Err(_) => {
            let port: u16 = std::fs::read_to_string(dir.join("gui.port"))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .ok_or_else(|| std::io::Error::other("the running Qorral did not record its port"))?;
            allow_foreground();
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
            s.set_write_timeout(Some(Duration::from_secs(2)))?;
            s.write_all(b"activate\n")?;
            Ok(Instance::Handed)
        }
    }
}

/// Serves hand-overs: calls `on_activate` for each valid request.
pub fn serve(listener: TcpListener, on_activate: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(conn).take(256).read_line(&mut line).is_ok() && line.trim() == "activate" {
                on_activate();
            }
        }
    });
}

/// Lets the first instance take the foreground (Windows only grants it to
/// the process the user just started, unless that process passes it on).
fn allow_foreground() {
    #[cfg(windows)]
    {
        const ASFW_ANY: u32 = u32::MAX;
        // SAFETY: a plain call without pointers.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(ASFW_ANY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn second_claim_hands_over_to_the_first() {
        let dir = std::env::temp_dir().join(format!("qorral-single-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let Instance::First { _lock, listener } = claim(&dir).unwrap() else { panic!("first claim must win") };
        let (tx, rx) = mpsc::channel();
        serve(listener, move || {
            let _ = tx.send(());
        });
        assert!(matches!(claim(&dir).unwrap(), Instance::Handed));
        rx.recv_timeout(Duration::from_secs(5)).expect("the first instance was asked to activate");
        drop(_lock);
        assert!(matches!(claim(&dir).unwrap(), Instance::First { .. }), "the lock is free once the first is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
