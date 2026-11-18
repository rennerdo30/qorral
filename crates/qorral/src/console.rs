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
    /// Remote session: guest pixels changed; the UI thread draws them (`Console::draw_cpu`).
    Frame(String),
    /// The guest's pointer image: RGBA rows, size, hotspot.
    Cursor { id: String, rgba: Vec<u8>, width: u16, height: u16, hot_x: u16, hot_y: u16 },
    /// The guest shows or hides its pointer.
    CursorVisible(String, bool),
}

#[cfg(windows)]
mod imp {
    use super::ConsoleEvent;
    use qorral_core::Library;
    use qorral_core::qmp::Qmp;
    use qorral_display::{Button, Caps, Display, Event, SharedMap};
    use qorral_gpu::{Compositor, GuestMemory, ImageParams, Layout, NativeWindow, PresentMode};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub type Notify = Arc<dyn Fn(ConsoleEvent) + Send + Sync>;

    /// `PIXMAN_x8r8g8b8` and `PIXMAN_a8r8g8b8`: 32-bit words 0xXXRRGGBB, the backing store's own
    /// layout. The only formats the CPU path draws.
    const XRGB: [u32; 2] = [0x2002_0888, 0x2002_8888];
    /// Behind the guest, where it does not fill its area (the compositor's clear colour).
    const CLEAR: u32 = 0x0F0F12;

    /// Remote sessions: the guest's pixels in host memory, drawn by the software renderer.
    /// QEMU's own shared mapping when it offers one (nothing is copied before drawing), else a
    /// copy of the pixels sent inside the messages.
    struct CpuGuest {
        map: Option<SharedMap>,
        own: Vec<u8>,
        width: u32,
        height: u32,
        stride: u32,
        /// Guest rectangles changed since the UI thread last drew (x, y, w, h).
        damage: Vec<[i32; 4]>,
    }

    impl CpuGuest {
        fn bytes(&self) -> &[u8] {
            self.map.as_ref().map_or(&self.own, |m| m.bytes())
        }

        fn damage(&mut self, x: i32, y: i32, w: i32, h: i32) {
            // Many small rectangles cost more to scale one by one than their bounding box.
            if self.damage.len() >= 16 {
                let b = self.damage.iter().fold([x, y, x + w, y + h], |b, r| {
                    [b[0].min(r[0]), b[1].min(r[1]), b[2].max(r[0] + r[2]), b[3].max(r[1] + r[3])]
                });
                self.damage = vec![[b[0], b[1], b[2] - b[0], b[3] - b[1]]];
            } else {
                self.damage.push([x, y, w, h]);
            }
        }
    }

    /// Everything the frame thread and the UI thread share.
    struct Shared {
        /// Remote session: guest pixels come through host memory, not the GPU.
        remote: bool,
        cpu: Option<CpuGuest>,
        /// A `Frame` wake-up is on its way to the UI thread (one at a time).
        cpu_wake: bool,
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
        /// QORRAL_TRACE: flip received to presented, per frame (ms), and that frame's parts
        /// (acquire, GPU, present call).
        trace: Option<Vec<f32>>,
        parts: Vec<[f32; 3]>,
        last_timing: Option<qorral_gpu::PresentTiming>,
    }

    impl Shared {
        /// The guest fitted into the area, centred, aspect kept. A remote session never scales
        /// up: 1:1 stays sharp, costs plain row copies and sends the remote link no blur.
        fn dst(&self) -> [f32; 4] {
            let a = self.area;
            if self.guest.0 == 0 || self.guest.1 == 0 {
                return a;
            }
            let mut k = (a[2] / self.guest.0 as f32).min(a[3] / self.guest.1 as f32);
            if self.remote {
                k = k.min(1.0);
            }
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
            self.last_timing = c.present(guest, dst).ok();
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
        /// The mouse is captured: relative motion only, every button to the guest.
        captured: bool,
    }

    impl Console {
        /// Starts attaching to machine `id` (on a worker); `area` is where its screen goes. In a
        /// `remote` session the guest's pixels come through host memory and the UI thread draws
        /// them ([`Console::draw_cpu`]); otherwise the GPU shows them zero copy.
        pub fn open(
            id: &str,
            root: PathBuf,
            window: (isize, isize),
            size: (u32, u32),
            area: [f32; 4],
            remote: bool,
            notify: Notify,
        ) -> Console {
            let shared = Arc::new(Mutex::new(Shared {
                remote,
                cpu: None,
                cpu_wake: false,
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
                trace: std::env::var_os("QORRAL_TRACE").map(|_| Vec::with_capacity(300)),
                parts: Vec::new(),
                last_timing: None,
                failed: false,
            }));
            let (sh, id_owned) = (shared.clone(), id.to_string());
            std::thread::spawn(move || {
                let r = attach(&root, &id_owned, sh, notify.clone());
                notify(ConsoleEvent::Attached(id_owned, r));
            });
            Console { id: id.to_string(), shared, display: None, held: BTreeSet::new(), inside: false, captured: false }
        }

        pub fn set_display(&mut self, d: Display) {
            self.display = Some(d);
        }

        /// The compositor owns the window's pixels (the CPU path must not present).
        pub fn is_presenting(&self) -> bool {
            self.shared.lock().is_ok_and(|s| s.comp.is_some())
        }

        /// Guest pixels changed that [`Console::draw_cpu`] has not drawn yet.
        pub fn cpu_pending(&self) -> bool {
            self.shared.lock().is_ok_and(|s| s.cpu.as_ref().is_some_and(|c| !c.damage.is_empty()))
        }

        /// Remote session: draws the guest's changed pixels (all of them, and the background
        /// around the guest, with `full`) into `dst`, a `w` x `h` backing store, fitted into the
        /// guest area. Returns the window rectangles it wrote (x, y, w, h).
        pub fn draw_cpu(&self, dst: &mut [u32], w: usize, h: usize, full: bool) -> Vec<[i32; 4]> {
            let Ok(mut s) = self.shared.lock() else { return Vec::new() };
            s.cpu_wake = false;
            let (fit, area) = (s.dst(), s.area);
            let Some(g) = s.cpu.as_mut() else { return Vec::new() };
            let damage = std::mem::take(&mut g.damage);
            let (gw, gh) = (g.width as i32, g.height as i32);
            if gw == 0 || gh == 0 || dst.len() < w * h || fit[2] < 1.0 || fit[3] < 1.0 {
                return Vec::new();
            }
            let clip = |r: [f32; 4]| {
                let (x0, y0) = ((r[0] as i32).max(0), (r[1] as i32).max(0));
                let (x1, y1) = (((r[0] + r[2]) as i32).min(w as i32), ((r[1] + r[3]) as i32).min(h as i32));
                (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
            };
            let f = [fit[0] as i32, fit[1] as i32, fit[2] as i32, fit[3] as i32];
            let Some(fc) = clip(fit) else { return Vec::new() };
            let mut out = Vec::new();
            let rects: Vec<[i32; 4]> = if full {
                // The whole guest area: the bars around the fitted guest in the background.
                if let Some(a) = clip(area) {
                    for y in a[1]..a[1] + a[3] {
                        let row = y as usize * w;
                        dst[row + a[0] as usize..row + (a[0] + a[2]) as usize].fill(CLEAR);
                    }
                    out.push(a);
                }
                vec![fc]
            } else {
                // Each guest rectangle in window pixels, a pixel wider for the filter's reach.
                let (kx, ky) = (f[2] as f32 / gw as f32, f[3] as f32 / gh as f32);
                let (x1, y1) = (fc[0] + fc[2], fc[1] + fc[3]);
                damage
                    .iter()
                    .filter_map(|r| {
                        let rx0 = ((r[0] as f32 * kx).floor() as i32 + f[0] - 1).max(fc[0]);
                        let ry0 = ((r[1] as f32 * ky).floor() as i32 + f[1] - 1).max(fc[1]);
                        let rx1 = (((r[0] + r[2]) as f32 * kx).ceil() as i32 + f[0] + 1).min(x1);
                        let ry1 = (((r[1] + r[3]) as f32 * ky).ceil() as i32 + f[1] + 1).min(y1);
                        (rx1 > rx0 && ry1 > ry0).then_some([rx0, ry0, rx1 - rx0, ry1 - ry0])
                    })
                    .collect()
            };
            let src = Src { bytes: g.bytes(), width: g.width, height: g.height, stride: g.stride };
            for r in &rects {
                scale_into(&src, dst, w, f, *r);
            }
            if out.is_empty() {
                out = rects;
            }
            out
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

        /// Captures the mouse (relative motion, every button to the guest) or releases it.
        pub fn set_captured(&mut self, on: bool) {
            self.captured = on;
        }

        /// Relative motion while the mouse is captured.
        pub fn mouse_by(&self, dx: i32, dy: i32) {
            if let (true, Some(d)) = (self.captured, &self.display) {
                let _ = d.mouse_by(dx, dy);
            }
        }

        /// Presents the last frame again (after a resize while the guest shows nothing new).
        pub fn redraw(&self) {
            if let Ok(mut s) = self.shared.lock() {
                s.present();
            }
        }

        /// The pointer moved; returns whether it is over the guest.
        pub fn pointer(&mut self, x: f64, y: f64) -> bool {
            if self.captured {
                return true; // relative motion only while captured
            }
            let p = self.guest_point(x, y);
            self.inside = p.is_some();
            if let (Some(d), Some((gx, gy))) = (&self.display, p) {
                let _ = d.mouse_to(gx, gy);
            }
            self.inside
        }

        /// A mouse button over the guest: sent, and true. Elsewhere: false (the UI's).
        pub fn button(&self, b: Button, down: bool) -> bool {
            if !self.inside && !self.captured {
                return false;
            }
            if let Some(d) = &self.display {
                let _ = d.button(b, down);
            }
            true
        }

        pub fn wheel(&self, up: bool) {
            if let (true, Some(d)) = (self.inside || self.captured, &self.display) {
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

    /// Guest pixels in host memory: `height` rows of `stride` bytes, 0xXXRRGGBB words.
    struct Src<'a> {
        bytes: &'a [u8],
        width: u32,
        height: u32,
        stride: u32,
    }

    impl Src<'_> {
        /// Row `y` as pixels (bounds checked once per row; empty if out of range).
        fn row(&self, y: u32) -> &[[u8; 4]] {
            let start = y as usize * self.stride as usize;
            let end = start + self.width as usize * 4;
            self.bytes.get(start..end).map_or(&[], |r| r.as_chunks::<4>().0)
        }
    }

    #[inline]
    fn rgb(p: [u8; 4]) -> u32 {
        u32::from_le_bytes(p) & 0xFF_FFFF
    }

    /// Draws window rectangle `r` (inside `fit`) of the guest fitted into `fit` (x, y, w, h)
    /// into `dst` (rows of `dw` pixels): a plain copy at 1:1, else bilinear with 8-bit weights.
    fn scale_into(s: &Src, dst: &mut [u32], dw: usize, fit: [i32; 4], r: [i32; 4]) {
        let (sw, sh) = (i64::from(s.width), i64::from(s.height));
        if i64::from(fit[2]) == sw && i64::from(fit[3]) == sh {
            let (gx, n) = ((r[0] - fit[0]) as usize, r[2] as usize);
            for y in r[1]..r[1] + r[3] {
                let src = s.row((y - fit[1]) as u32);
                let Some(src) = src.get(gx..gx + n) else { continue };
                let out = &mut dst[y as usize * dw + r[0] as usize..][..n];
                for (o, p) in out.iter_mut().zip(src) {
                    *o = rgb(*p);
                }
            }
            return;
        }
        // 16.16 fixed point: where each destination pixel's centre falls in the source, as the
        // two source pixels around it and the weight (of 256) of the second.
        let (step_x, step_y) = ((sw << 16) / i64::from(fit[2]), (sh << 16) / i64::from(fit[3]));
        let pos = |d: i32, step: i64, max: i64| -> (u32, u32, u32) {
            let p = ((i64::from(d) * 2 + 1) * step / 2 - (1 << 15)).clamp(0, (max - 1) << 16);
            let i0 = (p >> 16) as u32;
            (i0, (i0 + 1).min(max as u32 - 1), ((p >> 8) & 0xff) as u32)
        };
        let xs: Vec<(u32, u32, u32)> = (r[0]..r[0] + r[2]).map(|x| pos(x - fit[0], step_x, sw)).collect();
        for y in r[1]..r[1] + r[3] {
            let (y0, y1, fy) = pos(y - fit[1], step_y, sh);
            let (a, b) = (s.row(y0), s.row(y1));
            if a.len() < s.width as usize || b.len() < s.width as usize {
                continue;
            }
            let row = &mut dst[y as usize * dw + r[0] as usize..][..xs.len()];
            for (out, &(x0, x1, fx)) in row.iter_mut().zip(&xs) {
                let (x0, x1) = (x0 as usize, x1 as usize);
                let top = lerp(rgb(a[x0]), rgb(a[x1]), fx);
                let bottom = lerp(rgb(b[x0]), rgb(b[x1]), fx);
                *out = lerp(top, bottom, fy);
            }
        }
    }

    /// Blends two 0x00RRGGBB colours: `t` of 256 parts of `b`, the rest of `a`.
    #[inline]
    fn lerp(a: u32, b: u32, t: u32) -> u32 {
        let rb = (((a & 0xFF00FF) * (256 - t) + (b & 0xFF00FF) * t) >> 8) & 0xFF00FF;
        let g = (((a & 0xFF00) * (256 - t) + (b & 0xFF00) * t) >> 8) & 0xFF00;
        rb | g
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
        let remote = shared.lock().is_ok_and(|s| s.remote);
        // Remote sessions draw on the CPU: QEMU's shared memory (never the GPU path).
        let caps =
            if remote { Caps { map: true, ..Caps::default() } } else { Caps { vulkan: true, ..Caps::default() } };
        Display::attach(run.pid, 0, caps, &mut qmp, move |ev| {
            let Ok(mut s) = shared.lock() else { return };
            // Remote: new guest pixels; wake the UI thread once until it has drawn them.
            let wake = |s: &mut Shared| {
                if !s.cpu_wake {
                    s.cpu_wake = true;
                    notify(ConsoleEvent::Frame(id.clone()));
                }
                if !s.live {
                    s.live = true;
                    notify(ConsoleEvent::Live(id.clone()));
                }
            };
            match ev {
                Some(Event::ScanoutMap { handle, offset, width, height, stride, format }) => {
                    let len = stride as usize * height as usize;
                    if !XRGB.contains(&format) || u64::from(stride) < u64::from(width) * 4 {
                        close_handle(handle);
                        notify(ConsoleEvent::Failed(id.clone(), format!("unsupported guest pixel format {format:#x}")));
                        return;
                    }
                    // The handle is the map's from here (closed with it, also on failure).
                    match SharedMap::open(handle, offset, len) {
                        Ok(map) => {
                            if s.trace.is_some() {
                                eprintln!("qorral: console {id}: guest screen {width}x{height} read in place (shared memory)");
                            }
                            s.guest = (width, height);
                            s.cpu = Some(CpuGuest {
                                map: Some(map),
                                own: Vec::new(),
                                width,
                                height,
                                stride,
                                damage: vec![[0, 0, width as i32, height as i32]],
                            });
                            wake(&mut s);
                        }
                        Err(e) => notify(ConsoleEvent::Failed(id.clone(), format!("guest screen not readable: {e}"))),
                    }
                }
                Some(Event::UpdateMap { x, y, width, height }) => {
                    if let Some(g) = s.cpu.as_mut().filter(|g| g.map.is_some()) {
                        g.damage(x, y, width, height);
                        wake(&mut s);
                    }
                }
                Some(Event::Scanout { width, height, stride, format, data }) => {
                    let len = stride as usize * height as usize;
                    if XRGB.contains(&format) && u64::from(stride) >= u64::from(width) * 4 && data.len() >= len {
                        if s.trace.is_some() {
                            eprintln!("qorral: console {id}: guest screen {width}x{height} sent inside the messages");
                        }
                        s.guest = (width, height);
                        s.cpu = Some(CpuGuest {
                            map: None,
                            own: data[..len].to_vec(),
                            width,
                            height,
                            stride,
                            damage: vec![[0, 0, width as i32, height as i32]],
                        });
                        wake(&mut s);
                    }
                }
                Some(Event::Update { x, y, width, height, stride, format, data }) => {
                    let Some(g) = s.cpu.as_mut().filter(|g| g.map.is_none()) else { return };
                    let (x0, y0) = (x.max(0) as usize, y.max(0) as usize);
                    let w = (width.max(0) as usize).min((g.width as usize).saturating_sub(x0));
                    let h = (height.max(0) as usize).min((g.height as usize).saturating_sub(y0));
                    if !XRGB.contains(&format) || data.len() < stride as usize * h || (stride as usize) < w * 4 {
                        return;
                    }
                    let gs = g.stride as usize;
                    for row in 0..h {
                        let to = (y0 + row) * gs + x0 * 4;
                        let from = row * stride as usize;
                        if let (Some(d), Some(src)) = (g.own.get_mut(to..to + w * 4), data.get(from..from + w * 4)) {
                            d.copy_from_slice(src);
                        }
                    }
                    g.damage(x0 as i32, y0 as i32, w as i32, h as i32);
                    wake(&mut s);
                }
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
                    let t0 = std::time::Instant::now();
                    s.last = resource_id;
                    s.present();
                    if resource_id != 0 && s.comp.is_some() {
                        let (w, h) = s.guest;
                        // Reborrow out of the guard so the fields borrow separately.
                        let sh = &mut *s;
                        if let Some(t) = sh.trace.as_mut() {
                            if let Some(p) = sh.last_timing {
                                sh.parts.push([
                                    p.acquire.as_secs_f32() * 1e3,
                                    p.gpu.as_secs_f32() * 1e3,
                                    p.present.as_secs_f32() * 1e3,
                                ]);
                            }
                            t.push(t0.elapsed().as_secs_f32() * 1000.0);
                            if t.len() >= 300 {
                                let parts = std::mem::take(&mut sh.parts);
                                let med = |i: usize| {
                                    let mut v: Vec<f32> = parts.iter().map(|p| p[i]).collect();
                                    v.sort_by(f32::total_cmp);
                                    v.get(v.len() / 2).copied().unwrap_or(0.0)
                                };
                                eprintln!(
                                    "qorral: console {id}: medians acquire {:.3} ms, record + GPU {:.3} ms, present call {:.3} ms",
                                    med(0),
                                    med(1),
                                    med(2)
                                );
                                t.sort_by(f32::total_cmp);
                                eprintln!(
                                    "qorral: console {id} {w}x{h}: flip received to presented over {} frames: median {:.3} ms, p99 {:.3} ms, max {:.3} ms",
                                    t.len(),
                                    t[t.len() / 2],
                                    t[t.len() * 99 / 100],
                                    t[t.len() - 1]
                                );
                                t.clear();
                            }
                        }
                    }
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
                            id: id.clone(),
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
                    notify(ConsoleEvent::CursorVisible(id.clone(), visible));
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn guest(w: u32, h: u32, px: impl Fn(u32, u32) -> u32) -> Vec<u8> {
            let mut v = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    v.extend_from_slice(&(px(x, y) | 0xFF00_0000).to_le_bytes());
                }
            }
            v
        }

        #[test]
        fn one_to_one_is_a_copy_without_alpha() {
            let bytes = guest(4, 3, |x, y| x * 0x10 + y * 0x1000);
            let s = Src { bytes: &bytes, width: 4, height: 3, stride: 16 };
            let mut dst = vec![0u32; 6 * 5];
            scale_into(&s, &mut dst, 6, [1, 1, 4, 3], [1, 1, 4, 3]);
            assert_eq!(dst[6 + 1], 0);
            assert_eq!(dst[2 * 6 + 3], 2 * 0x10 + 0x1000);
            assert_eq!(dst[0], 0, "outside the rectangle untouched");
        }

        #[test]
        fn scaling_keeps_flat_colour_exact_and_blends_edges() {
            let bytes = guest(8, 8, |x, _| if x < 4 { 0x204060 } else { 0xC0E0FF });
            let s = Src { bytes: &bytes, width: 8, height: 8, stride: 32 };
            let mut dst = vec![0u32; 5 * 5];
            scale_into(&s, &mut dst, 5, [0, 0, 5, 5], [0, 0, 5, 5]);
            assert_eq!(dst[0], 0x204060, "flat areas stay exact");
            assert_eq!(dst[4], 0xC0E0FF);
            let mid = dst[2];
            assert!(mid != 0x204060 && mid != 0xC0E0FF, "the edge blends: {mid:#08x}");
            assert!((mid >> 16) > 0x20 && (mid >> 16) < 0xC0);
        }

        #[test]
        fn a_damage_rectangle_writes_only_inside() {
            let bytes = guest(16, 16, |_, _| 0x123456);
            let s = Src { bytes: &bytes, width: 16, height: 16, stride: 64 };
            let mut dst = vec![0u32; 10 * 10];
            scale_into(&s, &mut dst, 10, [0, 0, 10, 10], [2, 3, 4, 2]);
            for y in 0..10 {
                for x in 0..10 {
                    let inside = (2..6).contains(&x) && (3..5).contains(&y);
                    assert_eq!(dst[y * 10 + x] != 0, inside, "({x}, {y})");
                }
            }
        }
    }
}
