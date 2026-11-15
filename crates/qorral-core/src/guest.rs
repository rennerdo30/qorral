//! QEMU guest agent client (`qemu-ga` inside installed guests), over the
//! loopback socket QEMU exposes for the `org.qemu.guest_agent.0` port.
//!
//! Blocking, for worker threads, with a timeout on every socket operation.
//! The agent keeps one stream across clients, so a reply meant for an earlier
//! client can still be queued: every session starts with a
//! `guest-sync-delimited` handshake and drops everything before its token.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// Captured output larger than this is a protocol error, not unbounded memory.
const MAX_REPLY: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum AgentError {
    /// No agent answered: the guest is still booting or has no agent installed.
    Unreachable(String),
    Protocol(String),
    /// The agent refused the command (`{"error": ...}`).
    Command(String),
    /// The guest command did not finish in time; it may still be running.
    Timeout,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Unreachable(e) => write!(f, "guest agent not reachable: {e}"),
            AgentError::Protocol(e) => write!(f, "guest agent protocol: {e}"),
            AgentError::Command(e) => write!(f, "guest agent refused: {e}"),
            AgentError::Timeout => write!(f, "the guest command did not finish in time"),
        }
    }
}

impl std::error::Error for AgentError {}

/// What a guest command produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

pub struct GuestAgent {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl GuestAgent {
    /// Connects and synchronises. `timeout` bounds each socket operation.
    pub fn connect(port: u16, timeout: Duration) -> Result<GuestAgent, AgentError> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let unreachable = |e: std::io::Error| AgentError::Unreachable(e.to_string());
        let stream = TcpStream::connect_timeout(&addr, timeout).map_err(unreachable)?;
        stream.set_read_timeout(Some(timeout)).map_err(unreachable)?;
        stream.set_write_timeout(Some(timeout)).map_err(unreachable)?;
        let mut a = GuestAgent { stream, buf: Vec::new() };
        a.sync()?;
        Ok(a)
    }

    /// Waits up to `wait` for an agent to answer (a guest that is still booting).
    pub fn wait_connect(port: u16, wait: Duration) -> Result<GuestAgent, AgentError> {
        let end = Instant::now() + wait;
        loop {
            match GuestAgent::connect(port, Duration::from_secs(5)) {
                Ok(a) => return Ok(a),
                Err(e) if Instant::now() >= end => return Err(e),
                Err(_) => std::thread::sleep(Duration::from_secs(2)),
            }
        }
    }

    fn sync(&mut self) -> Result<(), AgentError> {
        let token = ((std::process::id() as u64) << 20) ^ (crate::supervisor::now() & 0xF_FFFF);
        self.send(&json!({ "execute": "guest-sync-delimited", "arguments": { "id": token } }))?;
        for _ in 0..64 {
            let m = self.read_message()?;
            if m.get("return").and_then(Value::as_u64) == Some(token) {
                return Ok(());
            }
            // Anything else is a stale reply for an earlier client: drop it.
        }
        Err(AgentError::Protocol("no sync reply among 64 queued messages".into()))
    }

    fn send(&mut self, msg: &Value) -> Result<(), AgentError> {
        let mut text = msg.to_string();
        text.push('\n');
        self.stream.write_all(text.as_bytes()).map_err(|e| AgentError::Unreachable(e.to_string()))
    }

    fn read_message(&mut self) -> Result<Value, AgentError> {
        loop {
            if let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=nl).collect();
                // guest-sync-delimited puts a raw 0xFF marker before its reply.
                let start = line.iter().position(|&b| b != 0xFF).unwrap_or(line.len());
                let text = String::from_utf8_lossy(&line[start..]);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                return serde_json::from_str(text).map_err(|e| AgentError::Protocol(format!("bad JSON: {e}")));
            }
            if self.buf.len() > MAX_REPLY {
                return Err(AgentError::Protocol("reply too large".into()));
            }
            let mut chunk = [0u8; 65536];
            let n = self.stream.read(&mut chunk).map_err(|e| match e.kind() {
                // QEMU accepts the connection even when no agent runs in the guest.
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    AgentError::Unreachable("no agent answered (guest still booting, or no agent installed)".into())
                }
                _ => AgentError::Unreachable(e.to_string()),
            })?;
            if n == 0 {
                return Err(AgentError::Unreachable("connection closed".into()));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Runs one agent command and returns its `return` value.
    pub fn execute(&mut self, command: &str, arguments: Option<Value>) -> Result<Value, AgentError> {
        let mut msg = json!({ "execute": command });
        if let Some(a) = arguments {
            msg["arguments"] = a;
        }
        self.send(&msg)?;
        let m = self.read_message()?;
        if let Some(r) = m.get("return") {
            return Ok(r.clone());
        }
        if let Some(e) = m.get("error") {
            return Err(AgentError::Command(e["desc"].as_str().unwrap_or("unknown error").to_string()));
        }
        Err(AgentError::Protocol(format!("unexpected message: {m}")))
    }

    /// Runs `argv` in the guest (as the agent's user: SYSTEM on Windows, root
    /// on Linux) and waits up to `timeout` for it to finish.
    pub fn exec(&mut self, argv: &[String], timeout: Duration) -> Result<ExecOutput, AgentError> {
        let (path, args) = argv.split_first().ok_or_else(|| AgentError::Command("no command given".into()))?;
        let r = self.execute("guest-exec", Some(json!({ "path": path, "arg": args, "capture-output": true })))?;
        let pid = r["pid"].as_i64().ok_or_else(|| AgentError::Protocol(format!("guest-exec gave no pid: {r}")))?;
        let end = Instant::now() + timeout;
        loop {
            let st = self.execute("guest-exec-status", Some(json!({ "pid": pid })))?;
            if st["exited"].as_bool() == Some(true) {
                return Ok(ExecOutput {
                    exit_code: st["exitcode"].as_i64().unwrap_or(-1),
                    stdout: decode(&st["out-data"]),
                    stderr: decode(&st["err-data"]),
                });
            }
            if Instant::now() >= end {
                return Err(AgentError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn decode(v: &Value) -> String {
    String::from_utf8_lossy(&base64_decode(v.as_str().unwrap_or(""))).into_owned()
}

/// Standard base64 (the agent's output encoding). Characters outside the
/// alphabet (padding, line breaks) are skipped, never a reason to fail.
pub fn base64_decode(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for v in s.bytes().filter_map(value) {
        acc = ((acc << 6) | u32::from(v)) & 0xFFFF;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    /// A scripted fake agent: one reply per received line. `{token}` in a
    /// reply becomes the id of the sync request, `<FF>` the raw 0xFF marker.
    fn fake_agent(replies: Vec<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut got = Vec::new();
            for reply in replies {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let v: Value = serde_json::from_str(&line).unwrap();
                let token = v["arguments"]["id"].as_u64().unwrap_or(0).to_string();
                got.push(line.trim().to_string());
                let text = reply.replace("{token}", &token);
                let mut bytes = Vec::new();
                for (i, part) in text.split("<FF>").enumerate() {
                    if i > 0 {
                        bytes.push(0xFF);
                    }
                    bytes.extend_from_slice(part.as_bytes());
                }
                w.write_all(&bytes).unwrap();
            }
            got
        });
        (port, h)
    }

    #[test]
    fn sync_skips_stale_replies_then_exec_captures_output() {
        let (port, h) = fake_agent(vec![
            // A stale reply from an earlier client arrives before ours.
            "{\"return\": {\"pid\": 7}}\n<FF>{\"return\": {token}}\n",
            "{\"return\": {\"pid\": 42}}\n",
            "{\"return\": {\"exited\": false}}\n",
            "{\"return\": {\"exited\": true, \"exitcode\": 3, \"out-data\": \"aGVsbG8K\", \"err-data\": \"b29wcw==\"}}\n",
        ]);
        let mut a = GuestAgent::connect(port, Duration::from_secs(5)).unwrap();
        let out = a.exec(&["cmd".into(), "/c".into(), "echo hello".into()], Duration::from_secs(10)).unwrap();
        assert_eq!(out, ExecOutput { exit_code: 3, stdout: "hello\n".into(), stderr: "oops".into() });
        let sent = h.join().unwrap();
        assert!(sent[0].contains("guest-sync-delimited"));
        assert!(sent[1].contains("\"capture-output\":true") && sent[1].contains("\"path\":\"cmd\""));
        assert!(sent[2].contains("\"pid\":42"));
    }

    #[test]
    fn refusals_are_command_errors() {
        let (port, h) = fake_agent(vec![
            "<FF>{\"return\": {token}}\n",
            "{\"error\": {\"class\": \"GenericError\", \"desc\": \"Guest agent command failed\"}}\n",
        ]);
        let mut a = GuestAgent::connect(port, Duration::from_secs(5)).unwrap();
        match a.exec(&["nope".into()], Duration::from_secs(5)) {
            Err(AgentError::Command(d)) => assert!(d.contains("failed")),
            other => panic!("expected a refusal, got {other:?}"),
        }
        h.join().unwrap();
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64_decode("aGVsbG8K"), b"hello\n");
        assert_eq!(base64_decode("b29wcw=="), b"oops");
        assert_eq!(base64_decode("+/8="), [0xFB, 0xFF]);
        assert!(base64_decode("").is_empty());
    }

    #[test]
    fn silent_socket_is_unreachable_not_a_hang() {
        // QEMU accepts connections even without an agent in the guest.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            drop(s);
        });
        let r = GuestAgent::connect(port, Duration::from_millis(500));
        assert!(matches!(r, Err(AgentError::Unreachable(_))), "got {:?}", r.err());
        h.join().unwrap();
    }
}
