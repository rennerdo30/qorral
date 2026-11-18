//! A D-Bus peer-to-peer connection: the client side of the authentication, then framed messages.
//! Blocking and thread-per-direction: one reader per connection, writers share a lock.

use crate::wire::{Malformed, Message};
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The peer refused the authentication or answered something unexpected.
    Auth(String),
    Malformed(Malformed),
    /// The peer answered a call with an error.
    Remote {
        name: String,
        text: String,
    },
    /// The peer closed the connection.
    Closed,
    /// QMP refused a command needed to attach.
    Qmp(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "display connection: {e}"),
            Error::Auth(s) => write!(f, "display connection refused: {s}"),
            Error::Malformed(m) => m.fmt(f),
            Error::Remote { name, text } => write!(f, "QEMU display: {name}: {text}"),
            Error::Closed => f.write_str("the display connection was closed"),
            Error::Qmp(s) => write!(f, "QEMU refused the display client: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        match e.kind() {
            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => {
                Error::Closed
            }
            _ => Error::Io(e),
        }
    }
}

impl From<Malformed> for Error {
    fn from(e: Malformed) -> Error {
        Error::Malformed(e)
    }
}

/// Client side of the D-Bus authentication (mechanism ANONYMOUS, which QEMU's display allows),
/// up to `BEGIN`. The reply is read byte by byte so no message bytes after it are consumed.
pub fn authenticate<S: Read + Write>(s: &mut S) -> Result<(), Error> {
    // The initial NUL byte, then AUTH with the trace string "qorral" in hex.
    s.write_all(b"\0AUTH ANONYMOUS 716f7272616c\r\n")?;
    s.flush()?;
    let line = read_line(s)?;
    if !line.starts_with("OK ") {
        return Err(Error::Auth(line));
    }
    s.write_all(b"BEGIN\r\n")?;
    s.flush()?;
    Ok(())
}

fn read_line<R: Read>(r: &mut R) -> Result<String, Error> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if line.len() > 1024 {
            return Err(Error::Auth("authentication line too long".into()));
        }
        r.read_exact(&mut b)?;
        line.push(b[0]);
    }
    line.truncate(line.len() - 2);
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Reads whole messages.
pub struct MsgReader<R: Read> {
    r: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: Read> MsgReader<R> {
    pub fn new(r: R) -> MsgReader<R> {
        MsgReader { r: BufReader::with_capacity(64 << 10, r), buf: Vec::new() }
    }

    pub fn read(&mut self) -> Result<Message, Error> {
        let mut prefix = [0u8; 16];
        self.r.read_exact(&mut prefix)?;
        let total = Message::length_from_prefix(&prefix)?;
        self.buf.clear();
        self.buf.extend_from_slice(&prefix);
        self.buf.resize(total, 0);
        self.r.read_exact(&mut self.buf[16..])?;
        Ok(Message::decode(&self.buf)?)
    }

    /// Reads until the reply to `serial`, skipping signals; turns an error reply into `Remote`.
    pub fn read_reply(&mut self, serial: u32) -> Result<Message, Error> {
        loop {
            let m = self.read()?;
            if m.reply_serial != Some(serial) {
                continue;
            }
            if m.kind == crate::wire::ERROR {
                let text = crate::wire::Reader::new(&m.body).str().unwrap_or("").to_string();
                return Err(Error::Remote { name: m.error_name.unwrap_or_default(), text });
            }
            return Ok(m);
        }
    }
}

/// Sends messages; shareable between threads.
pub struct MsgWriter<W: Write> {
    w: Mutex<W>,
    serial: AtomicU32,
}

impl<W: Write> MsgWriter<W> {
    pub fn new(w: W) -> MsgWriter<W> {
        MsgWriter { w: Mutex::new(w), serial: AtomicU32::new(1) }
    }

    /// Sends `m` with the next serial and returns that serial.
    pub fn send(&self, mut m: Message) -> Result<u32, Error> {
        m.serial = self.serial.fetch_add(1, Ordering::Relaxed);
        let data = m.encode();
        let mut w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(&data)?;
        w.flush()?;
        Ok(m.serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{METHOD_RETURN, Message, Writer};
    use std::net::{TcpListener, TcpStream};

    /// A fake D-Bus server: accepts ANONYMOUS, then echoes every call back as a return carrying
    /// the call's body.
    fn echo_server() -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut auth = Vec::new();
            let mut b = [0u8; 1];
            while !auth.ends_with(b"\r\n") {
                s.read_exact(&mut b).unwrap();
                auth.push(b[0]);
            }
            s.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n").unwrap();
            let mut begin = [0u8; 7];
            s.read_exact(&mut begin).unwrap();
            assert_eq!(&begin, b"BEGIN\r\n");
            let mut r = MsgReader::new(s.try_clone().unwrap());
            let call = r.read().unwrap();
            let mut reply = Message::reply_to(&call, &call.signature, call.body.clone());
            reply.serial = 77;
            // A signal first, which the client must skip.
            let mut sig = Message::call("/x", "a.b", "Changed", "", vec![]);
            sig.kind = crate::wire::SIGNAL;
            sig.serial = 76;
            s.write_all(&sig.encode()).unwrap();
            s.write_all(&reply.encode()).unwrap();
            auth
        });
        (port, h)
    }

    #[test]
    fn authenticates_and_gets_the_reply_to_its_call() {
        let (port, h) = echo_server();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        authenticate(&mut s).unwrap();
        let w = MsgWriter::new(s.try_clone().unwrap());
        let mut r = MsgReader::new(s);
        let mut body = Writer::default();
        body.u32(5);
        let serial = w.send(Message::call("/p", "i.f", "M", "u", body.buf.clone())).unwrap();
        let reply = r.read_reply(serial).unwrap();
        assert_eq!(reply.kind, METHOD_RETURN);
        assert_eq!(reply.body, body.buf);
        assert_eq!(h.join().unwrap(), b"\0AUTH ANONYMOUS 716f7272616c\r\n");
    }

    #[test]
    fn a_refused_authentication_is_an_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf).unwrap();
            s.write_all(b"REJECTED EXTERNAL\r\n").unwrap();
        });
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(matches!(authenticate(&mut s), Err(Error::Auth(l)) if l == "REJECTED EXTERNAL"));
        h.join().unwrap();
    }
}
