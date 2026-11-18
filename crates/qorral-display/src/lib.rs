//! Qorral's client for QEMU's D-Bus display (`-display dbus,p2p=yes`): guest frames in, keyboard
//! and mouse out.
//!
//! A small blocking implementation of the D-Bus subset the display uses, with no async runtime
//! and no allocation per frame on the shared-memory and shared-texture paths: the listener thread
//! answers each call as soon as the callback returns, because QEMU waits for some of those
//! answers before it renders the next frame.
//!
//! Attaching (Windows): two AF_UNIX socket pairs. One end of the first goes to QEMU through QMP
//! (`get-win32-socket`, `add_client @dbus-display`) and carries the calls to the console, the
//! keyboard and the mouse; one end of the second goes to QEMU in `Console.RegisterListener`, and
//! QEMU calls the listener on it.

pub mod conn;
pub mod listener;
pub mod wire;

#[cfg(windows)]
mod sock;

pub use conn::Error;
pub use listener::{BlobImage, Caps, Event};
#[cfg(windows)]
pub use sock::SharedMap;

const CONSOLE: &str = "org.qemu.Display1.Console";
const KEYBOARD: &str = "org.qemu.Display1.Keyboard";
const MOUSE: &str = "org.qemu.Display1.Mouse";

/// Mouse buttons as QEMU's display numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    WheelUp = 3,
    WheelDown = 4,
    Side = 5,
    Extra = 6,
}

/// Standard base64 with padding (QMP's encoding for `get-win32-socket`).
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One QMP command with string arguments, run by the caller's QMP connection.
pub type QmpCommand<'a> = &'a mut dyn FnMut(&str, &[(&str, &str)]) -> Result<(), String>;

#[cfg(windows)]
pub use imp::Display;

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::conn::{MsgReader, MsgWriter, authenticate};
    use crate::listener::handle;
    use crate::sock::{Stream, pair};
    use crate::wire::{Message, NO_REPLY_EXPECTED, Writer};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread::JoinHandle;

    /// A display client attached to one console of a running VM.
    pub struct Display {
        control: MsgWriter<Stream>,
        control_sock: Stream,
        listener_sock: Stream,
        console_path: String,
        threads: Vec<JoinHandle<()>>,
    }

    impl Display {
        /// Attaches to console `console` of the QEMU process `qemu_pid`. `on_event` runs on the
        /// listener thread for every display call; keep it short, QEMU may be waiting for the
        /// answer. It gets `None` once when the connection ends.
        pub fn attach(
            qemu_pid: u32,
            console: u32,
            caps: Caps,
            qmp: QmpCommand<'_>,
            mut on_event: impl FnMut(Option<Event<'_>>) + Send + 'static,
        ) -> Result<Display, Error> {
            static N: AtomicU32 = AtomicU32::new(0);

            // The control connection, handed to QEMU over QMP.
            let (ours, theirs) = pair()?;
            let info = theirs.duplicate_for(qemu_pid)?;
            let fdname = format!("qorral-display-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            qmp("get-win32-socket", &[("info", &base64(&info)), ("fdname", &fdname)]).map_err(Error::Qmp)?;
            qmp("add_client", &[("protocol", "@dbus-display"), ("fdname", &fdname)]).map_err(Error::Qmp)?;
            drop(theirs); // QEMU has its own copy now
            let control_sock = Stream(Arc::new(ours));
            authenticate(&mut control_sock.clone())?;
            let control = MsgWriter::new(control_sock.clone());
            let mut control_reader = MsgReader::new(control_sock.clone());

            // The listener connection: serve it before QEMU connects, since QEMU calls it
            // synchronously while it registers the listener.
            let (ours, theirs) = pair()?;
            let listener_info = theirs.duplicate_for(qemu_pid)?;
            let listener_sock = Stream(Arc::new(ours));
            let serve = listener_sock.clone();
            let listener_thread = std::thread::Builder::new().name("qorral-display".into()).spawn(move || {
                let mut s = serve.clone();
                if authenticate(&mut s).is_ok() {
                    let writer = MsgWriter::new(serve.clone());
                    let mut reader = MsgReader::new(serve);
                    while let Ok(m) = reader.read() {
                        if let Some(reply) = handle(&m, caps, &mut |e| on_event(Some(e)))
                            && writer.send(reply).is_err()
                        {
                            break;
                        }
                    }
                }
                on_event(None);
            })?;

            let console_path = format!("/org/qemu/Display1/Console_{console}");
            let mut body = Writer::default();
            body.bytes(&listener_info);
            let serial = control.send(Message::call(&console_path, CONSOLE, "RegisterListener", "ay", body.buf))?;
            control_reader.read_reply(serial)?;
            drop(theirs);

            // Drain the control connection (replies are not awaited, signals are not used), so
            // QEMU never blocks writing to it.
            let drain = std::thread::Builder::new()
                .name("qorral-display-control".into())
                .spawn(move || while control_reader.read().is_ok() {})?;
            Ok(Display { control, control_sock, listener_sock, console_path, threads: vec![listener_thread, drain] })
        }

        fn fire(&self, iface: &str, member: &str, sig: &str, body: impl FnOnce(&mut Writer)) -> Result<(), Error> {
            let mut w = Writer::default();
            body(&mut w);
            let mut m = Message::call(&self.console_path, iface, member, sig, w.buf);
            m.flags = NO_REPLY_EXPECTED;
            self.control.send(m).map(drop)
        }

        /// Presses or releases a key, given as a QEMU "qnum" (PC XT scancode set 1, 0xe0 prefix
        /// as 0x80 bit plus 0x80: e.g. right Ctrl is 0x9d).
        pub fn key(&self, qnum: u32, down: bool) -> Result<(), Error> {
            self.fire(KEYBOARD, if down { "Press" } else { "Release" }, "u", |w| w.u32(qnum))
        }

        /// Moves the pointer to `x, y` in guest display pixels (absolute devices).
        pub fn mouse_to(&self, x: u32, y: u32) -> Result<(), Error> {
            self.fire(MOUSE, "SetAbsPosition", "uu", |w| {
                w.u32(x);
                w.u32(y);
            })
        }

        /// Moves the pointer by `dx, dy` (relative devices, pointer capture).
        pub fn mouse_by(&self, dx: i32, dy: i32) -> Result<(), Error> {
            self.fire(MOUSE, "RelMotion", "ii", |w| {
                w.i32(dx);
                w.i32(dy);
            })
        }

        pub fn button(&self, b: Button, down: bool) -> Result<(), Error> {
            self.fire(MOUSE, if down { "Press" } else { "Release" }, "u", |w| w.u32(b as u32))
        }
    }

    impl Drop for Display {
        fn drop(&mut self) {
            self.control_sock.0.close();
            self.listener_sock.0.close();
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(super::base64(&[0xff, 0xfe, 0x00, 0x01]), "//4AAQ==");
    }
}
