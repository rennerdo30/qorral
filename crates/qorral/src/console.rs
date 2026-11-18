//! A machine's console in the main window: the guest's screen straight from its GPU memory
//! (zero copy, `qorral-gpu`) under the app's title row, keyboard and mouse to the guest.
//!
//! Threads: attaching runs on a worker (QMP and sockets); guest frames are presented on the
//! display's listener thread the moment they flip; the UI thread repaints the title row (the
//! overlay) and routes input. The compositor sits behind one lock shared by those threads.

#[cfg(windows)]
pub use imp::*;

/// What the console tells the UI thread.
pub enum ConsoleEvent {
    /// The display client is attached (or could not be).
    #[cfg(windows)]
    Attached(String, Result<qorral_display::Display, String>),
    /// The first guest frame is on screen.
    Live(String),
    /// The GPU path is not available; the message says why.
    Failed(String, String),
    /// QEMU closed the display connection (the machine stopped).
    Disconnected(String),
    /// The guest's pointer image: RGBA rows, size, hotspot.
    Cursor { rgba: Vec<u8>, width: u16, height: u16, hot_x: u16, hot_y: u16 },
    /// The guest shows or hides its pointer.
    CursorVisible(bool),
}

#[cfg(windows)]
mod imp {
    use super::ConsoleEvent;
    use qorral_core::Library;
    use qorral_core::qmp::Qmp;
    use qorral_display::{Button, Caps, Display, Event};
    use qorral_gpu::{Compositor, GuestMemory, ImageParams, Layout, NativeWindow, PresentMode};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub type Notify = Arc<dyn Fn(ConsoleEvent) + Send + Sync>;

    /// Everything the frame thread and the UI thread share.
    struct Shared {
        comp: Option<Compositor>,
        window: NativeWindow,
        size: (u32, u32),
        /// Where the guest goes (x, y, w, h in window pixels).
        area: [f32; 4],
        guest: (u32, u32),
        last: u32,
        /// The title row's pixels (rows of `size.0`), drawn over the guest.
        bar: Option<(Vec<u32>, u32, u32)>,
        bar_dirty: bool,
        resized: bool,
        live: bool,
        failed: bool,
        cursor_visible: Option<bool>,
    }

    impl Shared {
        /// The guest fitted into the area, centred, aspect kept.
        fn dst(&self) -> [f32; 4] {
            let a = self.area;
            if self.guest.0 == 0 || self.guest.1 == 0 {
                return a;
            }
            let k = (a[2] / self.guest.0 as f32).min(a[3] / self.guest.1 as f32);
            let (w, h) = ((self.guest.0 as f32 * k).round(), (self.guest.1 as f32 * k).round());
            [(a[0] + (a[2] - w) / 2.0).round(), (a[1] + (a[3] - h) / 2.0).round(), w, h]
        }

        /// Presents the last guest frame and the title row now (after a resize or a new title
        /// row). Errors are not fatal: the next frame tries again.
        fn present(&mut self) {
            let dst = self.dst();
            let (last, resized, size) = (self.last, self.resized, self.size);
            let bar = if self.bar_dirty { self.bar.take() } else { None };
            let Some(c) = self.comp.as_mut() else {
                if let Some(b) = bar {
                    self.bar = Some(b);
                }
                return;
            };
            if resized {
                let _ = c.resize(size.0, size.1);
                self.resized = false;
            }
            if let Some((px, w, h)) = &bar {
                let _ = c.set_overlay(px, *w, *h, (0, 0));
            }
            if let Some(b) = bar {
                self.bar = Some(b);
                self.bar_dirty = false;
            }
            let guest = (last != 0 && c.has(last)).then_some(last);
            let _ = c.present(guest, dst);
        }
    }

    pub struct Console {
        pub id: String,
        shared: Arc<Mutex<Shared>>,
        display: Option<Display>,
        /// Keys held down in the guest (released when the window loses focus or closes).
        held: BTreeSet<u32>,
        /// The pointer is over the guest.
        inside: bool,
    }

    impl Console {
        /// Starts attaching to machine `id` (on a worker); `area` is where its screen goes.
        pub fn open(
            id: &str,
            root: PathBuf,
            window: (isize, isize),
            size: (u32, u32),
            area: [f32; 4],
            notify: Notify,
        ) -> Console {
            let shared = Arc::new(Mutex::new(Shared {
                comp: None,
                window: NativeWindow { hwnd: window.0, hinstance: window.1 },
                size,
                area,
                guest: (0, 0),
                last: 0,
                bar: None,
                bar_dirty: false,
                resized: false,
                live: false,
                cursor_visible: None,
                failed: false,
            }));
            let (sh, id_owned) = (shared.clone(), id.to_string());
            std::thread::spawn(move || {
                let r = attach(&root, &id_owned, sh, notify.clone());
                notify(ConsoleEvent::Attached(id_owned, r));
            });
            Console { id: id.to_string(), shared, display: None, held: BTreeSet::new(), inside: false }
        }

        pub fn set_display(&mut self, d: Display) {
            self.display = Some(d);
        }

        /// The compositor owns the window's pixels (the CPU path must not present).
        pub fn is_presenting(&self) -> bool {
            self.shared.lock().is_ok_and(|s| s.comp.is_some())
        }

        /// A new title row (rows of `w` pixels): drawn over the guest from now on, and
        /// presented right away when the compositor runs.
        pub fn set_bar(&self, px: &[u32], w: u32, h: u32) {
            if let Ok(mut s) = self.shared.lock() {
                s.bar = Some((px.to_vec(), w, h));
                s.bar_dirty = true;
                s.present();
            }
        }

        pub fn resize(&self, size: (u32, u32), area: [f32; 4]) {
            if let Ok(mut s) = self.shared.lock() {
                s.size = size;
                s.area = area;
                s.resized = true;
            }
        }

        /// Guest pixel under window pixel (x, y), if the pointer is over the guest.
        fn guest_point(&self, x: f64, y: f64) -> Option<(u32, u32)> {
            let s = self.shared.lock().ok()?;
            let d = s.dst();
            let (fx, fy) = ((x as f32 - d[0]) / d[2], (y as f32 - d[1]) / d[3]);
            if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) || s.guest.0 == 0 {
                return None;
            }
            Some(((fx * s.guest.0 as f32) as u32, (fy * s.guest.1 as f32) as u32))
        }

        /// The pointer is over the guest (as of the last move).
        pub fn pointer_inside(&self) -> bool {
            self.inside
        }

        /// The pointer moved; returns whether it is over the guest.
        pub fn pointer(&mut self, x: f64, y: f64) -> bool {
            let p = self.guest_point(x, y);
            self.inside = p.is_some();
            if let (Some(d), Some((gx, gy))) = (&self.display, p) {
                let _ = d.mouse_to(gx, gy);
            }
            self.inside
        }

        /// A mouse button over the guest: sent, and true. Elsewhere: false (the UI's).
        pub fn button(&self, b: Button, down: bool) -> bool {
            if !self.inside {
                return false;
            }
            if let Some(d) = &self.display {
                let _ = d.button(b, down);
            }
            true
        }

        pub fn wheel(&self, up: bool) {
            if let (true, Some(d)) = (self.inside, &self.display) {
                let b = if up { Button::WheelUp } else { Button::WheelDown };
                let _ = d.button(b, true);
                let _ = d.button(b, false);
            }
        }

        /// A key, as a Windows scancode (0xE0xx for extended keys).
        pub fn key(&mut self, scancode: u32, down: bool) {
            let qnum = if scancode & 0xff00 == 0xe000 { 0x80 | (scancode & 0x7f) } else { scancode & 0xff };
            if down {
                self.held.insert(qnum);
            } else {
                self.held.remove(&qnum);
            }
            if let Some(d) = &self.display {
                let _ = d.key(qnum, down);
            }
        }

        /// Releases every key held in the guest (the window lost focus).
        pub fn release_keys(&mut self) {
            if let Some(d) = &self.display {
                for k in std::mem::take(&mut self.held) {
                    let _ = d.key(k, false);
                }
            }
        }

        pub fn ctrl_alt_del(&self) {
            if let Some(d) = &self.display {
                for k in [0x1d, 0x38, 0xd3] {
                    let _ = d.key(k, true);
                }
                std::thread::sleep(Duration::from_millis(30));
                for k in [0xd3, 0x38, 0x1d] {
                    let _ = d.key(k, false);
                }
            }
        }
    }

    impl Drop for Console {
        fn drop(&mut self) {
            self.release_keys();
            // Detach first (it stops the frame thread), then free the swapchain so the CPU
            // path can draw the window again. Never with the lock held: the frame thread takes it.
            drop(self.display.take());
            let comp = self.shared.lock().ok().and_then(|mut s| s.comp.take());
            drop(comp);
        }
    }

    fn attach(root: &std::path::Path, id: &str, shared: Arc<Mutex<Shared>>, notify: Notify) -> Result<Display, String> {
        let lib = Library::new(root);
        let e = lib.list().into_iter().find(|e| e.id == id).ok_or_else(|| format!("{id} no longer exists"))?;
        let run = e.run.ok_or_else(|| format!("{} is not running", e.config.name))?;
        let mut q = Qmp::connect(run.qmp_port, Duration::from_secs(5)).map_err(|e| e.to_string())?;
        let mut qmp = |cmd: &str, args: &[(&str, &str)]| -> Result<(), String> {
            let a: serde_json::Map<String, serde_json::Value> =
                args.iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string()))).collect();
            q.execute(cmd, Some(serde_json::Value::Object(a))).map(drop).map_err(|e| e.to_string())
        };
        let id = id.to_string();
        Display::attach(run.pid, 0, Caps { vulkan: true, ..Caps::default() }, &mut qmp, move |ev| {
            let Ok(mut s) = shared.lock() else { return };
            match ev {
                Some(Event::ScanoutBlob {
                    resource_id,
                    handle,
                    allocation_size,
                    memory_type_index,
                    device_uuid,
                    offset,
                    width,
                    height,
                    stride,
                    image,
                    ..
                }) => {
                    if s.comp.is_none() && !s.failed {
                        match Compositor::for_window(&device_uuid, s.window, s.size.0, s.size.1, PresentMode::Immediate)
                        {
                            Ok(c) => {
                                s.comp = Some(c);
                                s.bar_dirty = s.bar.is_some();
                            }
                            Err(err) => {
                                s.failed = true;
                                notify(ConsoleEvent::Failed(id.clone(), err.to_string()));
                            }
                        }
                    }
                    s.guest = (width, height);
                    let mem = GuestMemory {
                        handle: handle as isize,
                        allocation_size,
                        memory_type_index,
                        layout: Layout { offset, width, height, stride },
                        image: image.map(|i| ImageParams {
                            flags: i.flags,
                            image_type: i.image_type,
                            format: i.format,
                            width: i.width,
                            height: i.height,
                            depth: i.depth,
                            mip_levels: i.mip_levels,
                            array_layers: i.array_layers,
                            samples: i.samples,
                            tiling: i.tiling,
                            usage: i.usage,
                            sharing_mode: i.sharing_mode,
                        }),
                    };
                    match s.comp.as_mut() {
                        Some(c) => {
                            if let Err(err) = c.import(resource_id, mem) {
                                eprintln!("qorral: guest buffer {resource_id} not shown: {err}");
                            }
                        }
                        // No compositor: the handle is ours to close.
                        None => close_handle(handle),
                    }
                }
                Some(Event::FlipBlob { resource_id, .. }) => {
                    s.last = resource_id;
                    s.present();
                    if resource_id != 0 && s.comp.is_some() && !s.live {
                        s.live = true;
                        notify(ConsoleEvent::Live(id.clone()));
                    }
                }
                Some(Event::ForgetBlob { resource_id }) => {
                    if let Some(c) = s.comp.as_mut() {
                        c.forget(resource_id);
                    }
                }
                Some(Event::CursorDefine { width, height, hot_x, hot_y, data }) => {
                    let (w, h) = (width.clamp(0, 256) as usize, height.clamp(0, 256) as usize);
                    if w > 0 && h > 0 && data.len() >= w * h * 4 {
                        // QEMU's cursor pixels are 32-bit ARGB words: bytes B, G, R, A.
                        let rgba = data[..w * h * 4]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .flat_map(|p| [p[2], p[1], p[0], p[3]])
                            .collect();
                        notify(ConsoleEvent::Cursor {
                            rgba,
                            width: w as u16,
                            height: h as u16,
                            hot_x: hot_x.clamp(0, w as i32 - 1) as u16,
                            hot_y: hot_y.clamp(0, h as i32 - 1) as u16,
                        });
                    }
                }
                Some(Event::MouseSet { visible, .. }) if s.cursor_visible != Some(visible) => {
                    s.cursor_visible = Some(visible);
                    notify(ConsoleEvent::CursorVisible(visible));
                }
                None => notify(ConsoleEvent::Disconnected(id.clone())),
                _ => {}
            }
        })
        .map_err(|e| e.to_string())
    }

    fn close_handle(h: u64) {
        // SAFETY: a handle QEMU duplicated into this process for us.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h as usize as _) };
    }
}
