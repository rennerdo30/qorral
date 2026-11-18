//! The display listener object QEMU calls with frames: `/org/qemu/Display1/Listener`.
//!
//! [`handle`] turns one incoming call into an [`Event`] and returns the reply to send. Pixel data
//! sent inside a message is lent to the callback, not copied.

use crate::wire::{Message, Reader, Writer};

pub const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";
pub const LISTENER: &str = "org.qemu.Display1.Listener";
pub const WIN32_MAP: &str = "org.qemu.Display1.Listener.Win32.Map";
pub const WIN32_D3D11: &str = "org.qemu.Display1.Listener.Win32.D3d11";
/// Qorral's QEMU extension: zero-copy scanouts in exported host Vulkan memory.
pub const WIN32_VULKAN: &str = "org.qemu.Display1.Listener.Win32.VulkanMemory";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const PEER: &str = "org.freedesktop.DBus.Peer";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";

/// The optional ways of receiving frames this client offers QEMU. Without either, frames arrive
/// inside the messages (`Scanout`, `Update`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    /// Shared memory: `ScanoutMap` / `UpdateMap`.
    pub map: bool,
    /// Shared Direct3D 11 textures: `ScanoutTexture` / `UpdateTexture`.
    pub d3d11: bool,
    /// Exported host Vulkan memory (Qorral's QEMU): `ScanoutBlob` / `FlipBlob` / `ForgetBlob`.
    pub vulkan: bool,
}

impl Caps {
    fn interfaces(self) -> Vec<&'static str> {
        let mut v = vec![LISTENER];
        if self.map {
            v.push(WIN32_MAP);
        }
        if self.d3d11 {
            v.push(WIN32_D3D11);
        }
        if self.vulkan {
            v.push(WIN32_VULKAN);
        }
        v
    }
}

/// What QEMU tells the listener. Pixel formats are pixman format codes (`PIXMAN_x8r8g8b8` is
/// `0x20020888`).
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// The whole display, in the message: `height` rows of `stride` bytes.
    Scanout { width: u32, height: u32, stride: u32, format: u32, data: &'a [u8] },
    /// A changed rectangle, in the message: `height` rows of `stride` bytes.
    Update { x: i32, y: i32, width: i32, height: i32, stride: u32, format: u32, data: &'a [u8] },
    /// The display now lives in a shared file mapping. `handle` was duplicated into this process:
    /// the receiver owns it and must close it (see `SharedMap`).
    ScanoutMap { handle: u64, offset: u32, width: u32, height: u32, stride: u32, format: u32 },
    /// A changed rectangle of the current shared mapping.
    UpdateMap { x: i32, y: i32, width: i32, height: i32 },
    /// The display now lives in a shared D3D11 texture (an NT handle owned by the receiver, to be
    /// opened with `ID3D11Device1::OpenSharedResource1`; use keyed mutex 0 while reading). The
    /// visible part is `x, y, width, height`.
    ScanoutTexture {
        handle: u64,
        texture_width: u32,
        texture_height: u32,
        y0_top: bool,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    /// A changed rectangle of the current shared texture.
    UpdateTexture { x: i32, y: i32, width: i32, height: i32 },
    /// A guest scanout buffer in host Vulkan memory, to import once (zero copy): an NT handle
    /// (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_WIN32_BIT`, owned by the receiver) to the whole
    /// allocation, imported with exactly `allocation_size` and `memory_type_index` on the device
    /// whose `deviceUUID` is `device_uuid`. The pixels start at `offset`, `stride` bytes per row.
    /// `dedicated` allocations are read through a buffer. Sent again when the layout changes.
    ScanoutBlob {
        resource_id: u32,
        handle: u64,
        allocation_size: u64,
        memory_type_index: u32,
        dedicated: bool,
        device_uuid: [u8; 16],
        driver_uuid: [u8; 16],
        offset: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        /// The dedicated image the memory belongs to, as the host driver created it, if known.
        /// The driver may compress image memory: read it through an identical image.
        image: Option<BlobImage>,
    },
    /// Buffer `resource_id` is shown now; the rectangle changed. 0: blob scanouts ended, the
    /// other events carry the display again.
    FlipBlob { resource_id: u32, x: i32, y: i32, width: i32, height: i32 },
    /// The guest freed this buffer: release its import.
    ForgetBlob { resource_id: u32 },
    /// The display is off (for example the guest blanked it).
    Disable,
    /// The guest pointer's position and visibility.
    MouseSet { x: i32, y: i32, visible: bool },
    /// A new pointer image: `width * height` 32-bit pixels.
    CursorDefine { width: i32, height: i32, hot_x: i32, hot_y: i32, data: &'a [u8] },
}

/// A guest image exactly as the host driver created it (Vulkan enum and flag values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobImage {
    pub flags: u32,
    pub image_type: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub samples: u32,
    pub tiling: u32,
    pub usage: u32,
    pub sharing_mode: u32,
    pub handle_types: u32,
}

impl BlobImage {
    fn from_words(w: &[u32]) -> Option<BlobImage> {
        let &[
            flags,
            image_type,
            format,
            width,
            height,
            depth,
            mip_levels,
            array_layers,
            samples,
            tiling,
            usage,
            sharing_mode,
            handle_types,
            ..,
        ] = w
        else {
            return None;
        };
        Some(BlobImage {
            flags,
            image_type,
            format,
            width,
            height,
            depth,
            mip_levels,
            array_layers,
            samples,
            tiling,
            usage,
            sharing_mode,
            handle_types,
        })
    }
}

/// Handles one message received on the listener connection: calls `on_event` for display calls
/// and returns the reply to send, if the caller wants one.
pub fn handle(call: &Message, caps: Caps, on_event: &mut dyn FnMut(Event<'_>)) -> Option<Message> {
    if call.kind != crate::wire::METHOD_CALL {
        return None;
    }
    let reply = match dispatch(call, caps, on_event) {
        Ok(r) => r,
        Err(e) => Message::error_to(call, e.0, e.1),
    };
    call.wants_reply().then_some(reply)
}

type CallError = (&'static str, &'static str);
const INVALID_ARGS: CallError = ("org.freedesktop.DBus.Error.InvalidArgs", "unexpected arguments");

fn dispatch(call: &Message, caps: Caps, on_event: &mut dyn FnMut(Event<'_>)) -> Result<Message, CallError> {
    let iface = call.interface.as_deref().unwrap_or("");
    let member = call.member.as_deref().unwrap_or("");
    let sig = call.signature.as_str();
    let mut r = Reader::new(&call.body);
    let empty = || Message::reply_to(call, "", Vec::new());
    let bad = |_| INVALID_ARGS;

    let event = match (iface, member, sig) {
        (PROPERTIES, "GetAll", "s") => {
            let of = r.str().map_err(bad)?;
            let mut w = Writer::default();
            w.array(8, |w| {
                if of == LISTENER {
                    w.pad(8);
                    w.str("Interfaces");
                    w.sig("as");
                    w.strs(&caps.interfaces());
                }
            });
            return Ok(Message::reply_to(call, "a{sv}", w.buf));
        }
        (PROPERTIES, "Get", "ss") => {
            let (of, name) = (r.str().map_err(bad)?, r.str().map_err(bad)?);
            if of != LISTENER || name != "Interfaces" {
                return Err(("org.freedesktop.DBus.Error.UnknownProperty", "no such property"));
            }
            let mut w = Writer::default();
            w.sig("as");
            w.strs(&caps.interfaces());
            return Ok(Message::reply_to(call, "v", w.buf));
        }
        (PEER, "Ping", "") => return Ok(empty()),
        (INTROSPECTABLE, "Introspect", "") => {
            let mut xml = String::from("<node>");
            for i in caps.interfaces() {
                xml.push_str(&format!("<interface name=\"{i}\"/>"));
            }
            xml.push_str("</node>");
            let mut w = Writer::default();
            w.str(&xml);
            return Ok(Message::reply_to(call, "s", w.buf));
        }
        (LISTENER, "Scanout", "uuuuay") => Event::Scanout {
            width: r.u32().map_err(bad)?,
            height: r.u32().map_err(bad)?,
            stride: r.u32().map_err(bad)?,
            format: r.u32().map_err(bad)?,
            data: r.bytes().map_err(bad)?,
        },
        (LISTENER, "Update", "iiiiuuay") => Event::Update {
            x: r.i32().map_err(bad)?,
            y: r.i32().map_err(bad)?,
            width: r.i32().map_err(bad)?,
            height: r.i32().map_err(bad)?,
            stride: r.u32().map_err(bad)?,
            format: r.u32().map_err(bad)?,
            data: r.bytes().map_err(bad)?,
        },
        (LISTENER, "Disable", "") => Event::Disable,
        (LISTENER, "MouseSet", "iii") => {
            Event::MouseSet { x: r.i32().map_err(bad)?, y: r.i32().map_err(bad)?, visible: r.i32().map_err(bad)? != 0 }
        }
        (LISTENER, "CursorDefine", "iiiiay") => Event::CursorDefine {
            width: r.i32().map_err(bad)?,
            height: r.i32().map_err(bad)?,
            hot_x: r.i32().map_err(bad)?,
            hot_y: r.i32().map_err(bad)?,
            data: r.bytes().map_err(bad)?,
        },
        (WIN32_MAP, "ScanoutMap", "tuuuuu") if caps.map => Event::ScanoutMap {
            handle: r.u64().map_err(bad)?,
            offset: r.u32().map_err(bad)?,
            width: r.u32().map_err(bad)?,
            height: r.u32().map_err(bad)?,
            stride: r.u32().map_err(bad)?,
            format: r.u32().map_err(bad)?,
        },
        (WIN32_MAP, "UpdateMap", "iiii") if caps.map => Event::UpdateMap {
            x: r.i32().map_err(bad)?,
            y: r.i32().map_err(bad)?,
            width: r.i32().map_err(bad)?,
            height: r.i32().map_err(bad)?,
        },
        (WIN32_D3D11, "ScanoutTexture2d", "tuubuuuu") if caps.d3d11 => Event::ScanoutTexture {
            handle: r.u64().map_err(bad)?,
            texture_width: r.u32().map_err(bad)?,
            texture_height: r.u32().map_err(bad)?,
            y0_top: r.bool().map_err(bad)?,
            x: r.u32().map_err(bad)?,
            y: r.u32().map_err(bad)?,
            width: r.u32().map_err(bad)?,
            height: r.u32().map_err(bad)?,
        },
        (WIN32_VULKAN, "ScanoutBlob", "uttuuayaytuuuuau") if caps.vulkan => {
            let uuid = |r: &mut Reader<'_>| -> Result<[u8; 16], CallError> {
                r.bytes().map_err(bad)?.try_into().map_err(|_| INVALID_ARGS)
            };
            Event::ScanoutBlob {
                resource_id: r.u32().map_err(bad)?,
                handle: r.u64().map_err(bad)?,
                allocation_size: r.u64().map_err(bad)?,
                memory_type_index: r.u32().map_err(bad)?,
                dedicated: r.u32().map_err(bad)? & 1 != 0,
                device_uuid: uuid(&mut r)?,
                driver_uuid: uuid(&mut r)?,
                offset: r.u64().map_err(bad)?,
                width: r.u32().map_err(bad)?,
                height: r.u32().map_err(bad)?,
                stride: r.u32().map_err(bad)?,
                format: r.u32().map_err(bad)?,
                image: BlobImage::from_words(&r.u32s().map_err(bad)?),
            }
        }
        (WIN32_VULKAN, "FlipBlob", "uiiii") if caps.vulkan => Event::FlipBlob {
            resource_id: r.u32().map_err(bad)?,
            x: r.i32().map_err(bad)?,
            y: r.i32().map_err(bad)?,
            width: r.i32().map_err(bad)?,
            height: r.i32().map_err(bad)?,
        },
        (WIN32_VULKAN, "ForgetBlob", "u") if caps.vulkan => Event::ForgetBlob { resource_id: r.u32().map_err(bad)? },
        (WIN32_D3D11, "UpdateTexture2d", "iiii") if caps.d3d11 => Event::UpdateTexture {
            x: r.i32().map_err(bad)?,
            y: r.i32().map_err(bad)?,
            width: r.i32().map_err(bad)?,
            height: r.i32().map_err(bad)?,
        },
        _ => return Err(("org.freedesktop.DBus.Error.UnknownMethod", "not implemented by this listener")),
    };
    on_event(event);
    Ok(empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ERROR, METHOD_RETURN, NO_REPLY_EXPECTED};

    fn call(iface: &str, member: &str, sig: &str, body: impl FnOnce(&mut Writer)) -> Message {
        let mut w = Writer::default();
        body(&mut w);
        let mut m = Message::call(LISTENER_PATH, iface, member, sig, w.buf);
        m.serial = 3;
        m
    }

    #[test]
    fn properties_announce_the_offered_interfaces() {
        let caps = Caps { map: true, d3d11: false, vulkan: false };
        let reply = handle(&call(PROPERTIES, "GetAll", "s", |w| w.str(LISTENER)), caps, &mut |_| {}).unwrap();
        assert_eq!((reply.kind, reply.reply_serial, reply.signature.as_str()), (METHOD_RETURN, Some(3), "a{sv}"));
        let len = Reader::new(&reply.body).u32().unwrap() as usize;
        // The dict entry starts on 8: the array length is 4 bytes, then 4 bytes of padding.
        assert_eq!(len, reply.body.len() - 8);
        let mut r8 = Reader::new(&reply.body[8..]);
        assert_eq!(r8.str().unwrap(), "Interfaces");
        assert_eq!(r8.sig().unwrap(), "as");
        let n = r8.u32().unwrap() as usize;
        assert!(n > 0);
        let mut names = Vec::new();
        while !r8.at_end() {
            names.push(r8.str().unwrap().to_string());
        }
        assert_eq!(names, [LISTENER, WIN32_MAP]);
        // Other interfaces have no properties: an empty map, never an error (GDBus proxies call
        // GetAll for each interface they wrap).
        let reply = handle(&call(PROPERTIES, "GetAll", "s", |w| w.str(WIN32_MAP)), caps, &mut |_| {}).unwrap();
        // Length 0, then the padding to the 8-byte entry alignment, which the spec keeps even for
        // an empty array.
        assert_eq!(reply.body, [0u8; 8]);
    }

    #[test]
    fn display_calls_become_events_and_get_a_reply() {
        let caps = Caps { map: true, d3d11: true, vulkan: false };
        let mut seen = Vec::new();
        let m = call(WIN32_MAP, "ScanoutMap", "tuuuuu", |w| {
            w.u64(0x1234);
            w.u32(0);
            w.u32(1280);
            w.u32(800);
            w.u32(5120);
            w.u32(0x2002_0888);
        });
        let reply = handle(&m, caps, &mut |e| seen.push(format!("{e:?}"))).unwrap();
        assert_eq!(reply.kind, METHOD_RETURN);
        let m = call(LISTENER, "Update", "iiiiuuay", |w| {
            w.i32(1);
            w.i32(2);
            w.i32(1);
            w.i32(1);
            w.u32(4);
            w.u32(0x2002_0888);
            w.bytes(&[9, 8, 7, 6]);
        });
        handle(&m, caps, &mut |e| {
            assert_eq!(
                e,
                Event::Update { x: 1, y: 2, width: 1, height: 1, stride: 4, format: 0x2002_0888, data: &[9, 8, 7, 6] }
            )
        });
        assert_eq!(
            seen,
            ["ScanoutMap { handle: 4660, offset: 0, width: 1280, height: 800, stride: 5120, format: 537004168 }"]
        );
    }

    #[test]
    fn zero_copy_blob_calls_become_events() {
        let caps = Caps { vulkan: true, ..Caps::default() };
        let m = call(WIN32_VULKAN, "ScanoutBlob", "uttuuayaytuuuuau", |w| {
            w.u32(7);
            w.u64(0xabc);
            w.u64(16 << 20);
            w.u32(3);
            w.u32(1);
            w.bytes(&[1; 16]);
            w.bytes(&[2; 16]);
            w.u64(4096);
            w.u32(1280);
            w.u32(800);
            w.u32(5120);
            w.u32(0x2002_0888);
            w.array(4, |w| (1..=13).for_each(|v| w.u32(v)));
        });
        let mut got = None;
        handle(&m, caps, &mut |e| got = Some(format!("{e:?}")));
        let got = got.expect("an event");
        assert!(got.starts_with("ScanoutBlob { resource_id: 7, handle: 2748, allocation_size: 16777216, memory_type_index: 3, dedicated: true"));
        assert!(got.contains("offset: 4096, width: 1280, height: 800, stride: 5120, format: 537004168"));
        assert!(got.contains("image: Some(BlobImage { flags: 1, image_type: 2, format: 3"), "{got}");
        let m = call(WIN32_VULKAN, "FlipBlob", "uiiii", |w| {
            w.u32(7);
            (0..4).for_each(|i| w.i32(i));
        });
        handle(&m, caps, &mut |e| assert_eq!(e, Event::FlipBlob { resource_id: 7, x: 0, y: 1, width: 2, height: 3 }));
        // A UUID of the wrong length is refused.
        let m = call(WIN32_VULKAN, "ScanoutBlob", "uttuuayaytuuuuau", |w| {
            w.u32(7);
            w.u64(1);
            w.u64(1);
            w.u32(0);
            w.u32(0);
            w.bytes(&[1; 15]);
        });
        assert_eq!(handle(&m, caps, &mut |_| panic!("no event")).unwrap().kind, ERROR);
    }

    #[test]
    fn unoffered_and_malformed_calls_are_errors_without_events() {
        let caps = Caps::default();
        let mut events = 0;
        let m = call(WIN32_MAP, "UpdateMap", "iiii", |w| (0..4).for_each(|_| w.i32(0)));
        assert_eq!(handle(&m, caps, &mut |_| events += 1).unwrap().kind, ERROR);
        let m = call(LISTENER, "MouseSet", "iii", |w| w.i32(1)); // body too short
        assert_eq!(handle(&m, caps, &mut |_| events += 1).unwrap().kind, ERROR);
        assert_eq!(events, 0);
        let mut quiet = call(LISTENER, "Disable", "", |_| {});
        quiet.flags = NO_REPLY_EXPECTED;
        assert!(handle(&quiet, caps, &mut |_| events += 1).is_none());
        assert_eq!(events, 1);
    }
}
