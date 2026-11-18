//! The D-Bus wire format, as far as the display client needs it: little-endian messages, the
//! basic types, byte and string arrays, `a{sv}` property maps and the header fields.
//!
//! Alignment is relative to the start of the buffer being written or read. Header and body both
//! start on an 8-byte boundary of the message, so that is the same as message-relative alignment.

use std::fmt;

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

/// Header flag: the sender does not want a reply.
pub const NO_REPLY_EXPECTED: u8 = 0x1;

/// Largest message accepted (D-Bus allows 128 MiB; a 4K scanout without shared memory is 32 MiB).
pub const MAX_MESSAGE: usize = 128 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed D-Bus message: {}", self.0)
    }
}

impl std::error::Error for Malformed {}

/// Appends values with D-Bus alignment.
#[derive(Default, Debug)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn pad(&mut self, align: usize) {
        let n = self.buf.len().next_multiple_of(align);
        self.buf.resize(n, 0);
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.u32(u32::from(v));
    }
    pub fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.pad(8);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// `s` and `o`.
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    /// `g`.
    pub fn sig(&mut self, s: &str) {
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    /// `ay`.
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
    }
    /// An array whose elements are aligned to `align`; `f` writes the elements.
    pub fn array(&mut self, align: usize, f: impl FnOnce(&mut Writer)) {
        self.pad(4);
        let len_at = self.buf.len();
        self.buf.extend_from_slice(&[0; 4]);
        self.pad(align);
        let start = self.buf.len();
        f(self);
        let n = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&n.to_le_bytes());
    }
    /// `as`.
    pub fn strs(&mut self, v: &[&str]) {
        self.array(4, |w| v.iter().for_each(|s| w.str(s)));
    }
}

/// Reads values with D-Bus alignment.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }
    pub fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }
    fn align(&mut self, a: usize) -> Result<(), Malformed> {
        let p = self.pos.next_multiple_of(a);
        if p > self.buf.len() {
            return Err(Malformed("truncated"));
        }
        self.pos = p;
        Ok(())
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or(Malformed("truncated"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, Malformed> {
        self.align(4)?;
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap_or_default()))
    }
    pub fn i32(&mut self) -> Result<i32, Malformed> {
        Ok(self.u32()? as i32)
    }
    pub fn u64(&mut self) -> Result<u64, Malformed> {
        self.align(8)?;
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap_or_default()))
    }
    pub fn bool(&mut self) -> Result<bool, Malformed> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed("boolean not 0 or 1")),
        }
    }
    /// `s` and `o`.
    pub fn str(&mut self) -> Result<&'a str, Malformed> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        if self.u8()? != 0 {
            return Err(Malformed("string without NUL"));
        }
        std::str::from_utf8(s).map_err(|_| Malformed("string not UTF-8"))
    }
    /// `g`.
    pub fn sig(&mut self) -> Result<&'a str, Malformed> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        if self.u8()? != 0 {
            return Err(Malformed("signature without NUL"));
        }
        std::str::from_utf8(s).map_err(|_| Malformed("signature not ASCII"))
    }
    /// `ay`, borrowed from the message.
    pub fn bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    /// `au`.
    pub fn u32s(&mut self) -> Result<Vec<u32>, Malformed> {
        let n = self.u32()? as usize;
        if !n.is_multiple_of(4) {
            return Err(Malformed("array of u32 with a partial element"));
        }
        let bytes = self.take(n)?;
        Ok(bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect())
    }
    /// Skips one value of a basic type or a string array (enough for header fields and the
    /// simple property values QEMU sends).
    pub fn skip(&mut self, sig: &str) -> Result<(), Malformed> {
        match sig {
            "y" => self.u8().map(drop),
            "b" | "u" | "i" => self.u32().map(drop),
            "t" | "x" | "d" => self.u64().map(drop),
            "s" | "o" => self.str().map(drop),
            "g" => self.sig().map(drop),
            "ay" => self.bytes().map(drop),
            "as" => {
                let n = self.u32()? as usize;
                self.align(4)?;
                let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or(Malformed("truncated"))?;
                self.pos = end;
                Ok(())
            }
            _ => Err(Malformed("unsupported type")),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub reply_serial: Option<u32>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: String,
    pub body: Vec<u8>,
}

impl Message {
    pub fn call(path: &str, interface: &str, member: &str, signature: &str, body: Vec<u8>) -> Message {
        Message {
            kind: METHOD_CALL,
            path: Some(path.into()),
            interface: Some(interface.into()),
            member: Some(member.into()),
            signature: signature.into(),
            body,
            ..Message::default()
        }
    }

    /// A method return for `call`.
    pub fn reply_to(call: &Message, signature: &str, body: Vec<u8>) -> Message {
        Message {
            kind: METHOD_RETURN,
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            signature: signature.into(),
            body,
            ..Message::default()
        }
    }

    /// An error reply for `call`.
    pub fn error_to(call: &Message, name: &str, text: &str) -> Message {
        let mut w = Writer::default();
        w.str(text);
        Message {
            kind: ERROR,
            reply_serial: Some(call.serial),
            error_name: Some(name.into()),
            destination: call.sender.clone(),
            signature: "s".into(),
            body: w.buf,
            ..Message::default()
        }
    }

    pub fn wants_reply(&self) -> bool {
        self.kind == METHOD_CALL && self.flags & NO_REPLY_EXPECTED == 0
    }

    pub fn is(&self, interface: &str, member: &str) -> bool {
        self.interface.as_deref() == Some(interface) && self.member.as_deref() == Some(member)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.buf.reserve(128 + self.body.len());
        w.u8(b'l');
        w.u8(self.kind);
        w.u8(self.flags);
        w.u8(1);
        w.u32(self.body.len() as u32);
        w.u32(self.serial);
        w.array(8, |w| {
            let mut field = |code: u8, sig: &str, value: &dyn Fn(&mut Writer)| {
                w.pad(8);
                w.u8(code);
                w.sig(sig);
                value(w);
            };
            if let Some(v) = &self.path {
                field(1, "o", &|w| w.str(v));
            }
            if let Some(v) = &self.interface {
                field(2, "s", &|w| w.str(v));
            }
            if let Some(v) = &self.member {
                field(3, "s", &|w| w.str(v));
            }
            if let Some(v) = &self.error_name {
                field(4, "s", &|w| w.str(v));
            }
            if let Some(v) = self.reply_serial {
                field(5, "u", &|w| w.u32(v));
            }
            if let Some(v) = &self.destination {
                field(6, "s", &|w| w.str(v));
            }
            if let Some(v) = &self.sender {
                field(7, "s", &|w| w.str(v));
            }
            if !self.signature.is_empty() {
                field(8, "g", &|w| w.sig(&self.signature));
            }
        });
        w.pad(8);
        w.buf.extend_from_slice(&self.body);
        w.buf
    }

    /// Total message length from its first 16 bytes, or why it cannot be read.
    pub fn length_from_prefix(prefix: &[u8; 16]) -> Result<usize, Malformed> {
        if prefix[0] != b'l' {
            return Err(Malformed("big-endian messages are not supported"));
        }
        let body = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]) as usize;
        let fields = u32::from_le_bytes([prefix[12], prefix[13], prefix[14], prefix[15]]) as usize;
        let total = (16 + fields).next_multiple_of(8) + body;
        if total > MAX_MESSAGE {
            return Err(Malformed("message too large"));
        }
        Ok(total)
    }

    /// Parses one complete message (as sized by `length_from_prefix`).
    pub fn decode(data: &[u8]) -> Result<Message, Malformed> {
        let mut r = Reader::new(data);
        if r.u8()? != b'l' {
            return Err(Malformed("big-endian messages are not supported"));
        }
        let mut m = Message { kind: r.u8()?, flags: r.u8()?, ..Message::default() };
        if r.u8()? != 1 {
            return Err(Malformed("unknown protocol version"));
        }
        let body_len = r.u32()? as usize;
        m.serial = r.u32()?;
        let fields_len = r.u32()? as usize;
        let fields_end = 16usize.checked_add(fields_len).ok_or(Malformed("truncated"))?;
        if fields_end > data.len() {
            return Err(Malformed("truncated"));
        }
        while r.pos < fields_end {
            r.align(8)?;
            if r.pos >= fields_end {
                break;
            }
            let code = r.u8()?;
            let sig = r.sig()?;
            let text = |r: &mut Reader<'_>| -> Result<Option<String>, Malformed> { Ok(Some(r.str()?.to_string())) };
            match (code, sig) {
                (1, "o") => m.path = text(&mut r)?,
                (2, "s") => m.interface = text(&mut r)?,
                (3, "s") => m.member = text(&mut r)?,
                (4, "s") => m.error_name = text(&mut r)?,
                (5, "u") => m.reply_serial = Some(r.u32()?),
                (6, "s") => m.destination = text(&mut r)?,
                (7, "s") => m.sender = text(&mut r)?,
                (8, "g") => m.signature = r.sig()?.to_string(),
                (_, sig) => r.skip(sig)?,
            }
        }
        let body_start = fields_end.next_multiple_of(8);
        let body_end = body_start.checked_add(body_len).ok_or(Malformed("truncated"))?;
        if body_end > data.len() {
            return Err(Malformed("truncated"));
        }
        m.body = data[body_start..body_end].to_vec();
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_survives_encode_and_decode() {
        let mut w = Writer::default();
        w.u32(7);
        w.bytes(&[1, 2, 3]);
        w.u64(0x1122_3344_5566_7788);
        w.strs(&["a", "bc"]);
        let mut m =
            Message::call("/org/qemu/Display1/Console_0", "org.qemu.Display1.Console", "Probe", "uaytas", w.buf);
        m.serial = 42;
        let data = m.encode();
        let prefix: [u8; 16] = data[..16].try_into().unwrap();
        assert_eq!(Message::length_from_prefix(&prefix).unwrap(), data.len());
        let back = Message::decode(&data).unwrap();
        assert_eq!(back, m);
        let mut r = Reader::new(&back.body);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.bytes().unwrap(), [1, 2, 3]);
        assert_eq!(r.u64().unwrap(), 0x1122_3344_5566_7788);
        r.skip("as").unwrap();
        assert!(r.at_end());
    }

    #[test]
    fn known_bytes_from_the_spec_layout() {
        // A method return with reply serial 5 and no body: fixed header, one field, no padding
        // problems.
        let m = Message { kind: METHOD_RETURN, serial: 9, reply_serial: Some(5), ..Message::default() };
        let data = m.encode();
        assert_eq!(&data[..12], &[b'l', 2, 0, 1, 0, 0, 0, 0, 9, 0, 0, 0]);
        // Fields array: code 5, signature "u", padding to 4, value 5.
        assert_eq!(&data[12..16], &8u32.to_le_bytes());
        assert_eq!(&data[16..24], &[5, 1, b'u', 0, 5, 0, 0, 0]);
        assert_eq!(data.len(), 24);
    }

    #[test]
    fn truncated_and_hostile_input_is_refused() {
        let m = Message::call("/a", "b.c", "D", "u", vec![1, 0, 0, 0]).encode();
        for cut in [0, 5, 16, m.len() - 1] {
            assert!(Message::decode(&m[..cut]).is_err(), "cut at {cut}");
        }
        let mut huge = [0u8; 16];
        huge[0] = b'l';
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Message::length_from_prefix(&huge).is_err());
        assert!(Reader::new(&[5, 0, 0, 0, b'a']).str().is_err());
    }
}
