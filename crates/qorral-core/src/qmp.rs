//! QMP (QEMU Machine Protocol) client over loopback TCP.
//!
//! Blocking by design: callers are worker threads. Every socket operation has
//! a timeout, so a hung QEMU can never hang a worker forever. Asynchronous
//! events that arrive between replies are collected, not mistaken for replies.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

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
    timeout: Duration,
    /// Events received while waiting for replies, oldest first.
    pub events: Vec<Value>,
}

/// One input-to-display measurement from Qorral's QEMU (`x-qorral-latency-probe`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySample {
    /// From the key entering QEMU's input layer to the display update that changed the probe pixel.
    pub elapsed_us: u64,
    /// Display updates QEMU saw while waiting.
    pub updates: u64,
    pub timed_out: bool,
    /// Probe pixel brightness (0-255) before the key and at the last update read.
    pub before: u8,
    pub after: u8,
    /// Where the time went, from QEMU builds that mark the stages (`None` from older builds).
    pub stages: Option<LatencyStages>,
}

/// Stage marks of one measurement, in microseconds after the key press (0 = not seen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatencyStages {
    /// The guest first read from the keyboard device.
    pub key_read_us: u64,
    /// The guest's first present (a scanout flush or flip) after the key.
    pub first_present_us: u64,
    /// The guest's last present before the display update that carried the change.
    pub present_us: u64,
    /// That display update started (before QEMU's display listeners ran).
    pub update_start_us: u64,
    /// Presents (scanout flushes and flips) while waiting.
    pub presents: u64,
}

impl LatencyStages {
    /// The four spans of a measurement that ended at `elapsed_us`, when every mark was seen in
    /// order: key into QEMU to the guest reading it, the guest (input stack, app, rendering and
    /// presentation) up to its present, the host GPU path to the display update, and the display
    /// listeners.
    pub fn spans(&self, elapsed_us: u64) -> Option<[u64; 4]> {
        let marks = [0, self.key_read_us, self.present_us, self.update_start_us, elapsed_us];
        if marks[1..].contains(&0) || marks.windows(2).any(|w| w[1] < w[0]) {
            return None;
        }
        Some([marks[1], marks[2] - marks[1], marks[3] - marks[2], marks[4] - marks[3]])
    }
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
        let mut q = Qmp { reader: BufReader::new(stream), writer, timeout, events: Vec::new() };
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

    /// Waits for the next event called `name` (events that arrived earlier are checked first).
    pub fn wait_event(&mut self, name: &str, timeout: Duration) -> Result<Value, QmpError> {
        if let Some(i) = self.events.iter().position(|e| e["event"] == name) {
            return Ok(self.events.remove(i));
        }
        let deadline = Instant::now() + timeout;
        let result = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(QmpError::Protocol(format!("no {name} event within {} ms", timeout.as_millis())));
            }
            if let Err(e) = self.reader.get_ref().set_read_timeout(Some(left)) {
                break Err(e.into());
            }
            match self.read_message() {
                Ok(m) if m["event"] == name => break Ok(m),
                Ok(m) if m.get("event").is_some() => self.events.push(m),
                Ok(_) => {} // a stray reply; nothing is pending
                Err(QmpError::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => break Err(e),
            }
        };
        let _ = self.reader.get_ref().set_read_timeout(Some(self.timeout));
        result
    }

    /// Presses `key` (a QEMU qcode) and times until the guest's screen changes at its centre:
    /// dark to bright or bright to dark. Needs Qorral's QEMU build.
    pub fn latency_probe(&mut self, key: &str, timeout: Duration) -> Result<LatencySample, QmpError> {
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.execute("x-qorral-latency-probe", Some(json!({ "key": key, "timeout-ms": ms })))?;
        let ev = self.wait_event("QORRAL_LATENCY", timeout + Duration::from_secs(2))?;
        let d = &ev["data"];
        let byte = |v: &Value| u8::try_from(v.as_u64().unwrap_or(0).min(255)).unwrap_or(0);
        Ok(LatencySample {
            elapsed_us: d["elapsed-us"].as_u64().unwrap_or(0),
            updates: d["updates"].as_u64().unwrap_or(0),
            timed_out: d["timed-out"].as_bool().unwrap_or(true),
            before: byte(&d["before"]),
            after: byte(&d["after"]),
            stages: d.get("update-start-us").map(|_| {
                let us = |k: &str| d[k].as_u64().unwrap_or(0);
                LatencyStages {
                    key_read_us: us("key-read-us"),
                    first_present_us: us("first-present-us"),
                    present_us: us("present-us"),
                    update_start_us: us("update-start-us"),
                    presents: us("presents"),
                }
            }),
        })
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
    /// format; PNG needs a QEMU built with libpng, so PPM works with any build.
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

    #[test]
    fn latency_probe_reads_the_event() {
        let (port, h) = fake_qemu(vec![
            "{\"return\": {}}
",
            // An unrelated event first, then the probe's result after the command's reply.
            "{\"event\": \"RESUME\", \"timestamp\": {}}
{\"return\": {}}
{\"event\": \"QORRAL_LATENCY\", \"data\": {\"elapsed-us\": 4210, \"updates\": 3, \"timed-out\": false, \"before\": 0, \"after\": 255}}
",
        ]);
        let mut q = Qmp::connect(port, Duration::from_secs(5)).unwrap();
        let s = q.latency_probe("spc", Duration::from_secs(1)).unwrap();
        assert_eq!(
            s,
            LatencySample { elapsed_us: 4210, updates: 3, timed_out: false, before: 0, after: 255, stages: None }
        );
        assert_eq!(q.events.len(), 1, "the unrelated event is kept");
        let sent = h.join().unwrap();
        assert!(sent[1].contains("x-qorral-latency-probe") && sent[1].contains("\"spc\""));
    }

    #[test]
    fn latency_probe_reads_the_stage_marks() {
        let (port, h) = fake_qemu(vec![
            "{\"return\": {}}
",
            "{\"return\": {}}
{\"event\": \"QORRAL_LATENCY\", \"data\": {\"elapsed-us\": 9000, \"updates\": 2, \"timed-out\": false, \
             \"before\": 0, \"after\": 255, \"key-read-us\": 300, \"first-present-us\": 2000, \"present-us\": 6000, \
             \"update-start-us\": 8000, \"presents\": 2}}
",
        ]);
        let mut q = Qmp::connect(port, Duration::from_secs(5)).unwrap();
        let s = q.latency_probe("spc", Duration::from_secs(1)).unwrap();
        let st = s.stages.expect("stage marks");
        assert_eq!((st.key_read_us, st.first_present_us, st.presents), (300, 2000, 2));
        assert_eq!(st.spans(s.elapsed_us), Some([300, 5700, 2000, 1000]));
        h.join().unwrap();
    }

    #[test]
    fn latency_spans_need_every_mark_in_order() {
        let st =
            LatencyStages { key_read_us: 300, first_present_us: 0, present_us: 0, update_start_us: 800, presents: 0 };
        assert_eq!(st.spans(900), None, "no present seen");
        let st = LatencyStages {
            key_read_us: 300,
            first_present_us: 500,
            present_us: 900,
            update_start_us: 800,
            presents: 1,
        };
        assert_eq!(st.spans(1000), None, "present after the update");
    }
}
