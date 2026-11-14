//! QMP (QEMU Machine Protocol) client over loopback TCP.
//!
//! Blocking by design: callers are worker threads. Every socket operation has
//! a timeout, so a hung QEMU can never hang a worker forever. Asynchronous
//! events that arrive between replies are collected, not mistaken for replies.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Replies larger than this are a protocol error, not a reason to allocate.
const MAX_LINE: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum QmpError {
    Io(std::io::Error),
    Protocol(String),
    /// QEMU answered with `{"error": {...}}`.
    Command {
        class: String,
        desc: String,
    },
}

impl std::fmt::Display for QmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QmpError::Io(e) => write!(f, "QMP connection: {e}"),
            QmpError::Protocol(s) => write!(f, "QMP protocol: {s}"),
            QmpError::Command { class, desc } => write!(f, "QEMU refused: {desc} ({class})"),
        }
    }
}

impl std::error::Error for QmpError {}

impl From<std::io::Error> for QmpError {
    fn from(e: std::io::Error) -> Self {
        QmpError::Io(e)
    }
}

pub struct Qmp {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    /// Events received while waiting for replies, oldest first.
    pub events: Vec<Value>,
}

impl Qmp {
    /// Connects, reads the greeting and enters command mode.
    pub fn connect(port: u16, timeout: Duration) -> Result<Qmp, QmpError> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let writer = stream.try_clone()?;
        let mut q = Qmp { reader: BufReader::new(stream), writer, events: Vec::new() };
        let greeting = q.read_message()?;
        if greeting.get("QMP").is_none() {
            return Err(QmpError::Protocol(format!("unexpected greeting: {greeting}")));
        }
        q.execute("qmp_capabilities", None)?;
        Ok(q)
    }

    fn read_message(&mut self) -> Result<Value, QmpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = (&mut self.reader).take(MAX_LINE as u64).read_line(&mut line)?;
            if n == 0 {
                return Err(QmpError::Protocol("connection closed".into()));
            }
            if !line.ends_with('\n') && n >= MAX_LINE {
                return Err(QmpError::Protocol("message too large".into()));
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line).map_err(|e| QmpError::Protocol(format!("bad JSON: {e}")));
        }
    }

    /// Runs one command and returns its `return` value.
    pub fn execute(&mut self, command: &str, arguments: Option<Value>) -> Result<Value, QmpError> {
        let mut msg = json!({ "execute": command });
        if let Some(a) = arguments {
            msg["arguments"] = a;
        }
        let mut text = msg.to_string();
        text.push('\n');
        self.writer.write_all(text.as_bytes())?;
        loop {
            let m = self.read_message()?;
            if m.get("event").is_some() {
                self.events.push(m);
                continue;
            }
            if let Some(r) = m.get("return") {
                return Ok(r.clone());
            }
            if let Some(e) = m.get("error") {
                return Err(QmpError::Command {
                    class: e["class"].as_str().unwrap_or("Error").to_string(),
                    desc: e["desc"].as_str().unwrap_or("unknown error").to_string(),
                });
            }
            return Err(QmpError::Protocol(format!("unexpected message: {m}")));
        }
    }

    /// `running`, `paused`, `shutdown`, … (QEMU's RunState).
    pub fn status(&mut self) -> Result<String, QmpError> {
        let r = self.execute("query-status", None)?;
        Ok(r["status"].as_str().unwrap_or("unknown").to_string())
    }

    /// Asks the guest to shut down (ACPI power button).
    pub fn powerdown(&mut self) -> Result<(), QmpError> {
        self.execute("system_powerdown", None).map(|_| ())
    }

    /// Ends QEMU at once (the guest gets no chance to save anything).
    pub fn quit(&mut self) -> Result<(), QmpError> {
        self.execute("quit", None).map(|_| ())
    }

    /// Writes the guest screen to `path` as binary PPM. PPM is QEMU's built-in
    /// format; PNG needs a QEMU built with libpng, which ours is not.
    pub fn screendump_ppm(&mut self, path: &std::path::Path) -> Result<(), QmpError> {
        let p = path.to_string_lossy();
        self.execute("screendump", Some(json!({ "filename": p }))).map(|_| ())
    }

    /// Presses keys together, e.g. `["ctrl", "alt", "delete"]` (QEMU qcodes).
    pub fn send_keys(&mut self, qcodes: &[&str]) -> Result<(), QmpError> {
        let keys: Vec<Value> = qcodes.iter().map(|k| json!({ "type": "qcode", "data": k })).collect();
        self.execute("send-key", Some(json!({ "keys": keys }))).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A scripted fake QEMU: answers each received line with the next reply.
    fn fake_qemu(replies: Vec<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            w.write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\r\n").unwrap();
            let mut r = BufReader::new(s);
            let mut got = Vec::new();
            for reply in replies {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                got.push(line.trim().to_string());
                w.write_all(reply.as_bytes()).unwrap();
            }
            got
        });
        (port, h)
    }

    #[test]
    fn handshake_status_events_and_errors() {
        let (port, h) = fake_qemu(vec![
            "{\"return\": {}}\r\n",
            // An event arrives before the reply; it must not be taken as the reply.
            "{\"event\": \"RESUME\", \"timestamp\": {}}\r\n{\"return\": {\"status\": \"running\", \"running\": true}}\r\n",
            "{\"error\": {\"class\": \"GenericError\", \"desc\": \"no such key\"}}\r\n",
        ]);
        let mut q = Qmp::connect(port, Duration::from_secs(5)).unwrap();
        assert_eq!(q.status().unwrap(), "running");
        assert_eq!(q.events.len(), 1);
        match q.send_keys(&["nope"]) {
            Err(QmpError::Command { desc, .. }) => assert_eq!(desc, "no such key"),
            other => panic!("expected a command error, got {other:?}", other = other.err()),
        }
        let sent = h.join().unwrap();
        assert!(sent[0].contains("qmp_capabilities"));
        assert!(sent[1].contains("query-status"));
        assert!(sent[2].contains("\"qcode\""));
    }

    #[test]
    fn closed_connection_is_an_error_not_a_hang() {
        let (port, h) = fake_qemu(vec![]);
        // The fake closes after the greeting, so qmp_capabilities gets no reply.
        let r = Qmp::connect(port, Duration::from_secs(5));
        assert!(r.is_err());
        h.join().unwrap();
    }
}
