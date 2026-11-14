//! The window: winit for windows and input, softbuffer for damage-only CPU
//! presents. Event-driven: no frame loop; at idle nothing wakes up.
//!
//! The UI thread only handles input, layout and painting. Library reads,
//! QEMU starts and stops, screenshots and file pickers run on workers that
//! report back as [`UserEvent`]s.

use crate::ui::{Action, HostInfo, RunState, Thumb, Ui, VmView, load_fonts};
use crate::{GuiArgs, model, platform, trace};
use qorral_core::host::QemuInstall;
use qorral_core::qemu::Display;
use qorral_core::qmp::Qmp;
use qorral_core::supervisor;
use qorral_core::{Library, VmConfig};
use qorral_render::{Canvas, TextEngine};
use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

pub enum UserEvent {
    FontsReady,
    Host(HostInfo),
    Views(Vec<VmView>),
    Thumb(String, Thumb),
    Toast(Option<String>),
    Select(String),
    SmokeTimeout,
    /// The Windows frame changed the maximise button or window state.
    Frame,
    /// Another launch asked this window to come to the front.
    Activate,
}

type Proxy = Arc<Mutex<EventLoopProxy<UserEvent>>>;

fn send(p: &Proxy, e: UserEvent) {
    if let Ok(p) = p.lock() {
        let _ = p.send_event(e);
    }
}

struct Gfx {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    hwnd: isize,
}

struct App {
    t0: Instant,
    smoke: bool,
    smoke_reported: bool,
    ui: Option<Ui>,
    fonts: Option<std::thread::JoinHandle<Option<(TextEngine, crate::ui::Fonts)>>>,
    pending: Vec<UserEvent>,
    gfx: Option<Gfx>,
    size: (usize, usize),
    mods: ModifiersState,
    cursor: (i32, i32),
    hand: bool,
    proxy: Proxy,
    lib_root: std::path::PathBuf,
    qemu: Arc<Mutex<Option<QemuInstall>>>,
    exit_code: i32,
    /// Title row last sent to the frame's hit test.
    title_sent: Option<crate::ui::TitleGeometry>,
    closing: bool,
}

pub fn run(args: &GuiArgs, t0: Instant) -> i32 {
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("qorral: cannot open a window: {e}");
            return 1;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    trace(t0, "event loop built");
    let proxy: Proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
    // One window per user: a second launch hands over to the first and exits.
    // Agent smoke runs never take part.
    let mut _instance_lock = None;
    if !args.smoke {
        let dir = Library::default_root().parent().map(|p| p.to_path_buf()).unwrap_or_default();
        match crate::single::claim(&dir) {
            Ok(crate::single::Instance::Handed) => return 0,
            Ok(crate::single::Instance::First { _lock, listener }) => {
                let p = proxy.clone();
                crate::single::serve(listener, move || send(&p, UserEvent::Activate));
                _instance_lock = Some(_lock);
            }
            Err(e) => eprintln!("qorral: single-instance check failed ({e}); continuing"),
        }
        trace(t0, "single instance claimed");
    }
    let fonts_proxy = proxy.clone();
    let fonts = Some(std::thread::spawn(move || {
        let mut te = TextEngine::new();
        let f = load_fonts(&mut te).map(|f| (te, f));
        trace(t0, "fonts parsed (worker)");
        send(&fonts_proxy, UserEvent::FontsReady);
        f
    }));
    let lib_root = Library::default_root();
    let qemu = Arc::new(Mutex::new(None));
    if !args.smoke {
        start_watcher(proxy.clone(), lib_root.clone(), qemu.clone(), t0);
    } else {
        let p = proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            send(&p, UserEvent::SmokeTimeout);
        });
    }
    let mut app = App {
        t0,
        smoke: args.smoke,
        smoke_reported: false,
        ui: None,
        fonts,
        pending: Vec::new(),
        gfx: None,
        size: (0, 0),
        mods: ModifiersState::empty(),
        cursor: (0, 0),
        hand: false,
        proxy,
        lib_root,
        qemu,
        exit_code: 0,
        title_sent: None,
        closing: false,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("qorral: event loop failed: {e}");
        return 1;
    }
    app.exit_code
}

/// Finds QEMU once, then keeps the machine list and the live screens fresh.
/// Only changes are sent: an idle library costs no repaints.
fn start_watcher(proxy: Proxy, root: std::path::PathBuf, qemu: Arc<Mutex<Option<QemuInstall>>>, t0: Instant) {
    std::thread::spawn(move || {
        let q = QemuInstall::find();
        let host = HostInfo {
            accel: qorral_core::qemu::host_accel(),
            qemu_version: q.as_ref().and_then(|q| q.version()),
            qemu_found: q.is_some(),
        };
        if let Ok(mut g) = qemu.lock() {
            *g = q.clone();
        }
        send(&proxy, UserEvent::Host(host));
        let lib = Library::new(&root);
        let mut last_sig = String::new();
        let mut thumb_hash: std::collections::HashMap<String, u64> = Default::default();
        let mut tick = 0u64;
        loop {
            let remote = platform::is_remote_session();
            if tick.is_multiple_of(2) {
                let views = model::views(&lib, q.as_ref());
                let sig: String = views.iter().map(|v| format!("{}:{:?}:{};", v.id, v.state, v.name)).collect();
                if sig != last_sig {
                    last_sig = sig;
                    if tick == 0 {
                        trace(t0, "library read (worker)");
                    }
                    send(&proxy, UserEvent::Views(views));
                }
            }
            // Live screens: every second locally, every third second over RDP.
            if !remote || tick.is_multiple_of(3) {
                for e in lib.list() {
                    let Some(run) = e.run.as_ref().filter(|_| supervisor::state(&e) == supervisor::State::Running)
                    else {
                        continue;
                    };
                    let path = e.dir.join("thumb.ppm");
                    let ok =
                        Qmp::connect(run.qmp_port, Duration::from_secs(2)).and_then(|mut q| q.screendump_ppm(&path));
                    if ok.is_err() {
                        continue;
                    }
                    if let Some(t) = model::load_thumb(&path) {
                        let h = hash(&t.rgba);
                        if thumb_hash.get(&e.id) != Some(&h) {
                            thumb_hash.insert(e.id.clone(), h);
                            send(&proxy, UserEvent::Thumb(e.id.clone(), t));
                        }
                    }
                }
            }
            tick += 1;
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

fn hash(b: &[u8]) -> u64 {
    // FNV-1a over a sample: enough to notice a changed screen cheaply.
    let step = (b.len() / 65536).max(1);
    b.iter().step_by(step).fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
}

impl App {
    fn set_state(&mut self, id: &str, st: RunState) {
        if let Some(ui) = &mut self.ui {
            let mut vms = ui.vms.clone();
            if let Some(v) = vms.iter_mut().find(|v| v.id == id) {
                v.state = st;
            }
            ui.set_vms(vms);
        }
    }

    fn handle(&mut self, a: Action) {
        let (p, root) = (self.proxy.clone(), self.lib_root.clone());
        let qemu = self.qemu.clone();
        let toast = |p: &Proxy, msg: String| {
            send(p, UserEvent::Toast(Some(msg)));
            let p = p.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(8));
                send(&p, UserEvent::Toast(None));
            });
        };
        match a {
            Action::Start { id, window } => {
                self.set_state(&id, RunState::Starting);
                std::thread::spawn(move || {
                    let lib = Library::new(&root);
                    let q = qemu.lock().ok().and_then(|g| g.clone());
                    let r = match (q, lib.list().into_iter().find(|e| e.id == id)) {
                        (None, _) => {
                            Err("QEMU not found: set QORRAL_QEMU to the folder with qemu-system-x86_64".into())
                        }
                        (_, None) => Err(format!("{id} no longer exists")),
                        (Some(q), Some(e)) => {
                            let d = if window { Display::Window } else { Display::Headless };
                            supervisor::start(&lib, &q, &e, d, None).map(|_| ())
                        }
                    };
                    if let Err(e) = r {
                        toast(&p, e);
                    }
                    send(&p, UserEvent::Views(model::views(&lib, qemu.lock().ok().and_then(|g| g.clone()).as_ref())));
                });
            }
            Action::Stop { id, force } => {
                self.set_state(&id, RunState::Stopping);
                std::thread::spawn(move || {
                    let lib = Library::new(&root);
                    if let Some(e) = lib.list().into_iter().find(|e| e.id == id) {
                        let timeout = Duration::from_secs(if force { 10 } else { 90 });
                        match supervisor::stop(&e, force, timeout) {
                            Ok(supervisor::State::Running) => toast(
                                &p,
                                format!("{} ignored the shutdown request. Use Force off to end it.", e.config.name),
                            ),
                            Ok(_) => {}
                            Err(err) => toast(&p, err),
                        }
                    }
                    send(&p, UserEvent::Views(model::views(&lib, qemu.lock().ok().and_then(|g| g.clone()).as_ref())));
                });
            }
            Action::NewFromIso { preset } => {
                std::thread::spawn(move || {
                    let title = match preset.and_then(model::preset) {
                        Some(p) => format!("Choose the {} install image", p.label),
                        None => "Choose an install image".to_string(),
                    };
                    let Some(iso) = platform::pick_iso(&title) else { return };
                    let iso = std::path::PathBuf::from(iso);
                    let lib = Library::new(&root);
                    let q = qemu.lock().ok().and_then(|g| g.clone());
                    let Some(q) = q else {
                        toast(&p, "QEMU not found: set QORRAL_QEMU to the folder with qemu-system-x86_64".into());
                        return;
                    };
                    let name = model::default_name(preset, Some(&iso));
                    let cfg: VmConfig = model::config_for(&name, preset, Some(&iso));
                    match supervisor::create(&lib, &q, &cfg) {
                        Ok(e) => {
                            send(&p, UserEvent::Views(model::views(&lib, Some(&q))));
                            send(&p, UserEvent::Select(e.id));
                        }
                        Err(err) => toast(&p, err),
                    }
                });
            }
        }
    }

    fn apply(&mut self, ev: UserEvent) {
        let Some(ui) = &mut self.ui else {
            self.pending.push(ev);
            return;
        };
        match ev {
            UserEvent::Host(h) => {
                ui.host = h;
                ui.invalidate();
            }
            UserEvent::Views(v) => {
                ui.now = supervisor::now();
                ui.set_vms(v);
            }
            UserEvent::Thumb(id, t) => ui.set_thumb(&id, t),
            UserEvent::Toast(t) => ui.set_toast(t),
            UserEvent::Select(id) => {
                ui.selected = Some(id);
                let vms = ui.vms.clone();
                ui.set_vms(vms);
            }
            UserEvent::Activate => {
                if let Some(g) = &self.gfx {
                    g.window.set_minimized(false);
                    g.window.set_visible(true);
                    g.window.focus_window();
                }
            }
            UserEvent::Frame => {
                #[cfg(windows)]
                ui.set_frame_state(crate::frame::max_button_state(), crate::frame::is_zoomed());
            }
            UserEvent::FontsReady | UserEvent::SmokeTimeout => {}
        }
    }

    fn after_input(&mut self) {
        let win = self.ui.as_mut().and_then(|u| u.win_action.take());
        if let (Some(a), Some(g)) = (win, &self.gfx) {
            match a {
                crate::ui::WinAction::Minimize => g.window.set_minimized(true),
                crate::ui::WinAction::Close => self.closing = true,
            }
        }
        let actions = self.ui.as_mut().map(|u| u.take_actions()).unwrap_or_default();
        for a in actions {
            self.handle(a);
        }
        if let (Some(g), Some(ui)) = (&self.gfx, &self.ui) {
            if ui.pointer_cursor_is_hand() != self.hand {
                self.hand = ui.pointer_cursor_is_hand();
                g.window.set_cursor(if self.hand { CursorIcon::Pointer } else { CursorIcon::Default });
            }
            if ui.has_damage() {
                g.window.request_redraw();
            }
        }
    }

    fn render(&mut self) {
        let (Some(gfx), Some(ui)) = (&mut self.gfx, &mut self.ui) else { return };
        let (w, h) = self.size;
        if w == 0 || h == 0 || (!ui.has_damage() && ui.stats.presents > 0) {
            return;
        }
        let Ok(mut buf) = gfx.surface.buffer_mut() else { return };
        if buf.len() < w * h {
            return;
        }
        // A buffer of unknown age must be painted completely.
        let force = buf.age() == 0;
        let mut cv = Canvas::new(&mut buf[..], w, h);
        let painted = ui.paint(&mut cv, force);
        // The frame's hit test follows the painted title row.
        let geo = ui.title_geometry();
        if self.title_sent.as_ref() != Some(&geo) {
            #[cfg(windows)]
            crate::frame::set_caption(crate::frame::Caption {
                height: geo.height,
                controls: geo.controls.clone(),
                buttons_left: geo.buttons_left,
                max_button: geo.max_button,
            });
            self.title_sent = Some(geo);
        }
        let rects: Vec<softbuffer::Rect> = painted
            .iter()
            .filter_map(|r| {
                Some(softbuffer::Rect {
                    x: r.x.max(0) as u32,
                    y: r.y.max(0) as u32,
                    width: NonZeroU32::new(r.w.max(0) as u32)?,
                    height: NonZeroU32::new(r.h.max(0) as u32)?,
                })
            })
            .collect();
        if rects.is_empty() {
            return;
        }
        if let Err(e) = buf.present_with_damage(&rects) {
            eprintln!("qorral: present failed: {e}");
        }
        if ui.stats.first_frame_ms.is_none() {
            ui.stats.first_frame_ms = Some(self.t0.elapsed().as_secs_f64() * 1000.0);
            trace(self.t0, "first frame presented");
        }
        trace(
            self.t0,
            &format!(
                "present #{}: {} rect(s), {} px, paint {:.2} ms",
                ui.stats.presents,
                rects.len(),
                ui.stats.last_damage_px,
                ui.stats.last_paint_ms
            ),
        );
    }

    fn finish_start(&mut self) {
        let Some(t) = self.fonts.take() else { return };
        let Some((te, fonts)) = t.join().ok().flatten() else {
            eprintln!("qorral: no usable UI font found");
            self.exit_code = 1;
            return;
        };
        let scale = self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor() as f32);
        let mut ui = Ui::new(te, fonts, self.size.0, self.size.1, scale);
        ui.remote = platform::is_remote_session();
        ui.now = supervisor::now();
        self.ui = Some(ui);
        for ev in std::mem::take(&mut self.pending) {
            self.apply(ev);
        }
        trace(self.t0, "ui built");
        self.render();
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn smoke_check(&mut self, el: &ActiveEventLoop) {
        if !self.smoke || self.smoke_reported {
            return;
        }
        if let Some(ms) = self.ui.as_ref().and_then(|u| u.stats.first_frame_ms) {
            self.smoke_reported = true;
            let remote = self.ui.as_ref().is_some_and(|u| u.remote);
            println!("{{\"first_frame_ms\":{ms:.1},\"remote\":{remote}}}");
            el.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title("Qorral")
            .with_inner_size(LogicalSize::new(1440.0, 900.0))
            .with_min_inner_size(LogicalSize::new(720.0, 480.0))
            .with_visible(false);
        #[cfg(windows)]
        {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_drag_and_drop(false);
            if self.smoke {
                attrs = attrs.with_skip_taskbar(true).with_active(false);
            }
        }
        #[cfg(target_os = "macos")]
        {
            // The top bar runs under the traffic lights (Chrome::MacInset).
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs = attrs.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true);
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("qorral: cannot create the window: {e}");
                self.exit_code = 1;
                el.exit();
                return;
            }
        };
        trace(self.t0, "window created");
        let surface = match Context::new(window.clone()).and_then(|c| Surface::new(&c, window.clone())) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("qorral: cannot create a drawing surface: {e}");
                self.exit_code = 1;
                el.exit();
                return;
            }
        };
        let hwnd = match raw_window_handle::HasWindowHandle::window_handle(&*window).map(|h| h.as_raw()) {
            Ok(raw_window_handle::RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        };
        platform::dark_title_bar(hwnd);
        // Before reading the size: removing the caption grows the client area.
        #[cfg(windows)]
        {
            let p = self.proxy.clone();
            if !crate::frame::install(hwnd, move || send(&p, UserEvent::Frame)) {
                eprintln!("qorral: the modern frame could not be installed; using the system frame");
            }
        }
        let sz = window.inner_size();
        self.size = (sz.width as usize, sz.height as usize);
        let mut gfx = Gfx { window, surface, hwnd };
        if let (Some(w), Some(h)) = (NonZeroU32::new(sz.width), NonZeroU32::new(sz.height)) {
            let _ = gfx.surface.resize(w, h);
        }
        if self.smoke {
            if !platform::cloak_and_show(gfx.hwnd) {
                eprintln!("qorral: refusing --smoke without a cloaked window");
                self.exit_code = 1;
                el.exit();
                return;
            }
        } else {
            gfx.window.set_visible(true);
        }
        trace(self.t0, "shown");
        self.gfx = Some(gfx);
        if self.fonts.as_ref().is_some_and(|t| t.is_finished()) {
            self.finish_start();
            self.smoke_check(el);
        } else if let Some(g) = &mut self.gfx {
            // Fonts are still parsing: show the background now, the UI follows.
            let (w, h) = self.size;
            if let Ok(mut buf) = g.surface.buffer_mut()
                && buf.len() >= w * h
            {
                buf.fill(crate::theme::BG);
                let _ = buf.present();
                trace(self.t0, "background presented");
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::FontsReady => {
                if self.ui.is_none() && self.gfx.is_some() {
                    self.finish_start();
                    self.smoke_check(el);
                }
            }
            UserEvent::SmokeTimeout => {
                eprintln!("qorral: smoke run timed out");
                self.exit_code = 1;
                el.exit();
            }
            other => {
                self.apply(other);
                self.after_input();
                if self.closing {
                    el.exit();
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => {
                self.render();
                self.smoke_check(el);
            }
            WindowEvent::Resized(sz) => {
                self.size = (sz.width as usize, sz.height as usize);
                if let Some(g) = &mut self.gfx
                    && let (Some(w), Some(h)) = (NonZeroU32::new(sz.width), NonZeroU32::new(sz.height))
                {
                    let _ = g.surface.resize(w, h);
                }
                let scale = self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor() as f32);
                if let Some(ui) = &mut self.ui {
                    ui.resize(self.size.0, self.size.1, scale);
                }
                self.after_input();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(ui) = &mut self.ui {
                    ui.resize(self.size.0, self.size.1, scale_factor as f32);
                }
                self.after_input();
            }
            WindowEvent::Focused(focused) => {
                let remote = platform::is_remote_session();
                if let Some(ui) = &mut self.ui {
                    ui.set_active(focused);
                    if focused {
                        ui.set_remote(remote);
                    }
                }
                self.after_input();
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as i32, position.y as i32);
                if let Some(ui) = &mut self.ui {
                    ui.pointer_move(self.cursor.0, self.cursor.1);
                }
                self.after_input();
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(ui) = &mut self.ui {
                    ui.pointer_leave();
                }
                self.after_input();
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let (x, y) = self.cursor;
                if let Some(ui) = &mut self.ui {
                    match state {
                        ElementState::Pressed => ui.press(x, y),
                        ElementState::Released => ui.release(x, y),
                    }
                }
                self.after_input();
                if self.closing {
                    el.exit();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let ctrl = self.mods.control_key() || self.mods.super_key();
                if let Some(ui) = &mut self.ui {
                    match &event.logical_key {
                        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("k") => ui.key_focus_search(),
                        Key::Named(NamedKey::Escape) => ui.key_escape(),
                        Key::Named(NamedKey::Backspace) => ui.key_backspace(),
                        Key::Named(NamedKey::Enter) => ui.key_enter(),
                        Key::Named(NamedKey::ArrowLeft) => ui.key_move(-1),
                        Key::Named(NamedKey::ArrowRight) => ui.key_move(1),
                        Key::Named(NamedKey::ArrowUp) => ui.key_move(-2),
                        Key::Named(NamedKey::ArrowDown) => ui.key_move(2),
                        _ if !ctrl => {
                            if let Some(t) = &event.text {
                                ui.text(t);
                            }
                        }
                        _ => {}
                    }
                }
                self.after_input();
            }
            _ => {}
        }
    }
}
