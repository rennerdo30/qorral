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
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
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
    /// News from the open console.
    Console(crate::console::ConsoleEvent),
}

type Proxy = Arc<Mutex<EventLoopProxy<UserEvent>>>;

fn send(p: &Proxy, e: UserEvent) {
    if let Ok(p) = p.lock() {
        let _ = p.send_event(e);
    }
}

type WinSurface = Surface<Rc<Window>, Rc<Window>>;

struct Gfx {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    hwnd: isize,
}

struct App {
    t0: Instant,
    /// `--open`: the machine whose console to show once it runs.
    open: Option<String>,
    /// `--popout`: the machine whose console window to open once it runs.
    popout_arg: Option<String>,
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
    /// The open console (its machine's screen fills the window under the title row).
    #[cfg(windows)]
    console: Option<crate::console::Console>,
    /// The UI's pixels while a console is open (the compositor shows the title row from it).
    backing: Vec<u32>,
    /// Remote session: the console's guest area must be drawn completely on the next frame.
    console_full: bool,
    /// QORRAL_TRACE: CPU-path guest frames (draw and present, ms) and when the batch began.
    cpu_trace: Option<Vec<f32>>,
    cpu_trace_t0: Option<Instant>,
    /// The guest's pointer in the main window's console.
    gp: GuestPointer,
    /// Consoles in their own windows.
    #[cfg(windows)]
    popouts: Vec<PopOut>,
    /// Machines whose console should move into its own window (needs the event loop).
    popout_requests: Vec<String>,
}

/// The pointer over a console: over the guest, captured, the guest's own pointer.
struct GuestPointer {
    over: bool,
    captured: bool,
    cursor: Option<winit::window::CustomCursor>,
    visible: bool,
}

impl Default for GuestPointer {
    fn default() -> Self {
        GuestPointer { over: false, captured: false, cursor: None, visible: true }
    }
}

impl GuestPointer {
    /// Over the guest: the guest's pointer (or none when the guest hides it or the mouse is
    /// captured); elsewhere the UI's (`hand` over clickable things).
    fn apply(&self, window: &Window, hand: bool) {
        if self.captured {
            window.set_cursor_visible(false);
        } else if self.over {
            window.set_cursor_visible(self.visible);
            match &self.cursor {
                Some(c) => window.set_cursor(c.clone()),
                None => window.set_cursor(CursorIcon::Default),
            }
        } else {
            window.set_cursor_visible(true);
            window.set_cursor(if hand { CursorIcon::Pointer } else { CursorIcon::Default });
        }
    }
}

/// Captures the mouse into `window` for `console` (games: relative motion), or releases it.
#[cfg(windows)]
fn set_capture(window: &Window, console: &mut crate::console::Console, gp: &mut GuestPointer, on: bool) {
    use winit::window::CursorGrabMode;
    if on {
        if window.set_cursor_grab(CursorGrabMode::Confined).is_err() {
            return;
        }
    } else {
        let _ = window.set_cursor_grab(CursorGrabMode::None);
    }
    console.set_captured(on);
    gp.captured = on;
    gp.apply(window, false);
}

/// A host mouse button as the guest's (none for buttons the guest has no use for).
#[cfg(windows)]
fn guest_button(b: MouseButton) -> Option<qorral_display::Button> {
    use qorral_display::Button as B;
    Some(match b {
        MouseButton::Left => B::Left,
        MouseButton::Right => B::Right,
        MouseButton::Middle => B::Middle,
        MouseButton::Back => B::Side,
        MouseButton::Forward => B::Extra,
        MouseButton::Other(_) => return None,
    })
}

#[cfg(windows)]
fn wheel_up(d: &MouseScrollDelta) -> bool {
    match d {
        MouseScrollDelta::LineDelta(_, y) => *y > 0.0,
        MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
    }
}

/// Ctrl+Alt+G, the capture toggle (never sent to the guest).
fn is_capture_key(mods: ModifiersState, event: &winit::event::KeyEvent) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    mods.control_key() && mods.alt_key() && event.physical_key == PhysicalKey::Code(KeyCode::KeyG)
}

/// A machine's console in its own window: the guest fills it, the native frame around it.
#[cfg(windows)]
struct PopOut {
    window: Rc<Window>,
    console: crate::console::Console,
    gp: GuestPointer,
    /// Remote session: the window's software surface and pixels (no compositor there).
    cpu: Option<(WinSurface, Vec<u32>)>,
}

impl PopOut {
    /// Remote session: draws the guest's changed pixels (all of them with `full`) and
    /// presents only those.
    #[cfg(windows)]
    fn render_cpu(&mut self, full: bool) {
        let Some((surface, backing)) = self.cpu.as_mut() else { return };
        let sz = self.window.inner_size();
        let (w, h) = (sz.width as usize, sz.height as usize);
        if w == 0 || h == 0 {
            return;
        }
        let full = full || backing.len() != w * h;
        if backing.len() != w * h {
            *backing = vec![0; w * h];
            if let (Some(nw), Some(nh)) = (NonZeroU32::new(sz.width), NonZeroU32::new(sz.height)) {
                let _ = surface.resize(nw, nh);
            }
        }
        let rects = self.console.draw_cpu(backing, w, h, full);
        present_backing(surface, backing, w, h, &rects);
    }
}

/// Copies rectangles `rects` (x, y, w, h) of `backing` (`w` x `h`) into the window's buffer
/// and presents only them. A buffer of unknown age gets everything.
fn present_backing(
    surface: &mut Surface<Rc<Window>, Rc<Window>>,
    backing: &[u32],
    w: usize,
    h: usize,
    rects: &[[i32; 4]],
) {
    let Ok(mut buf) = surface.buffer_mut() else { return };
    if buf.len() < w * h || backing.len() < w * h {
        return;
    }
    let all = [[0, 0, w as i32, h as i32]];
    let rects = if buf.age() == 1 { rects } else { &all[..] };
    let mut damage = Vec::with_capacity(rects.len());
    for r in rects {
        let (x0, y0) = (r[0].clamp(0, w as i32) as usize, r[1].clamp(0, h as i32) as usize);
        let (x1, y1) = ((r[0] + r[2]).clamp(0, w as i32) as usize, (r[1] + r[3]).clamp(0, h as i32) as usize);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for y in y0..y1 {
            buf[y * w + x0..y * w + x1].copy_from_slice(&backing[y * w + x0..y * w + x1]);
        }
        if let (Some(dw), Some(dh)) = (NonZeroU32::new((x1 - x0) as u32), NonZeroU32::new((y1 - y0) as u32)) {
            damage.push(softbuffer::Rect { x: x0 as u32, y: y0 as u32, width: dw, height: dh });
        }
    }
    if !damage.is_empty()
        && let Err(e) = buf.present_with_damage(&damage)
    {
        eprintln!("qorral: present failed: {e}");
    }
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
        open: args.open.clone(),
        popout_arg: args.popout.clone(),
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
        #[cfg(windows)]
        console: None,
        backing: Vec::new(),
        console_full: false,
        cpu_trace: std::env::var_os("QORRAL_TRACE").map(|_| Vec::with_capacity(300)),
        cpu_trace_t0: Some(Instant::now()),
        gp: GuestPointer::default(),
        #[cfg(windows)]
        popouts: Vec::new(),
        popout_requests: Vec::new(),
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
                            let display = if window { Display::Window } else { Display::Headless };
                            let o = supervisor::StartOptions { display, ..Default::default() };
                            supervisor::start(&lib, &q, &e, &o).map(|_| ())
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
            Action::OpenConsole { id } => {
                // A machine shown in its own window: bring that window forward instead.
                #[cfg(windows)]
                if let Some(p) = self.popouts.iter().find(|p| p.console.id == id) {
                    p.window.focus_window();
                    if let Some(ui) = &mut self.ui {
                        ui.close_console();
                    }
                    return;
                }
                self.open_console(&id)
            }
            Action::PopOutConsole { id } => self.popout_requests.push(id),
            Action::CloseConsole => self.close_console(),
            Action::SendCtrlAltDel { id } => {
                #[cfg(windows)]
                if let Some(c) = self.console.as_ref().filter(|c| c.id == id) {
                    c.ctrl_alt_del();
                }
                #[cfg(not(windows))]
                let _ = id;
            }
            Action::Create(draft) => {
                let preset = draft.preset;
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
                    let name = draft.name.clone().unwrap_or_else(|| model::default_name(preset, Some(&iso)));
                    let mut cfg: VmConfig = model::config_for(&name, preset, Some(&iso));
                    // The words in the intent bar win over the preset.
                    if let Some(n) = draft.cpus {
                        cfg.cpus = n;
                    }
                    if let Some(n) = draft.memory_gib {
                        cfg.memory_mib = n * 1024;
                    }
                    if let Some(n) = draft.disk_gib {
                        cfg.disk_gib = n;
                    }
                    if let Some(g) = draft.gpu {
                        cfg.gpu = g;
                    }
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
            UserEvent::Select(id) => ui.select_id(&id),
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
            UserEvent::Console(ev) => self.console_event(ev),
            UserEvent::FontsReady | UserEvent::SmokeTimeout => {}
        }
    }

    fn open_console(&mut self, id: &str) {
        #[cfg(windows)]
        {
            let (Some(gfx), Some(ui)) = (&self.gfx, &self.ui) else { return };
            let hinstance = match raw_window_handle::HasWindowHandle::window_handle(&*gfx.window).map(|h| h.as_raw()) {
                Ok(raw_window_handle::RawWindowHandle::Win32(h)) => h.hinstance.map_or(0, |i| i.get()),
                _ => 0,
            };
            let g = ui.console_guest_rect();
            let p = self.proxy.clone();
            let notify: crate::console::Notify = Arc::new(move |ev| send(&p, UserEvent::Console(ev)));
            self.console = Some(crate::console::Console::open(
                id,
                self.lib_root.clone(),
                (gfx.hwnd, hinstance),
                (self.size.0 as u32, self.size.1 as u32),
                [g.x as f32, g.y as f32, g.w as f32, g.h as f32],
                ui.remote,
                notify,
            ));
            self.console_full = true;
        }
        #[cfg(not(windows))]
        {
            let _ = id;
            if let Some(ui) = &mut self.ui {
                ui.close_console();
                ui.set_toast(Some("The console is not available on this system yet.".into()));
            }
        }
    }

    fn close_console(&mut self) {
        #[cfg(windows)]
        if let (Some(g), Some(c)) = (&self.gfx, &mut self.console)
            && self.gp.captured
        {
            set_capture(&g.window, c, &mut self.gp, false);
        }
        #[cfg(windows)]
        drop(self.console.take());
        self.gp = GuestPointer::default();
        if let Some(ui) = &mut self.ui {
            ui.set_mouse_captured(false);
        }
        self.update_cursor();
        // The compositor drew the window: the CPU path repaints all of it.
        if let Some(ui) = &mut self.ui {
            ui.invalidate();
        }
    }

    fn console_event(&mut self, ev: crate::console::ConsoleEvent) {
        use crate::console::ConsoleEvent as E;
        // A console in its own window?
        #[cfg(windows)]
        {
            let id = match &ev {
                E::Attached(id, _) | E::Live(id) | E::Failed(id, _) | E::Disconnected(id) | E::Frame(id) => id.clone(),
                E::Cursor { id, .. } | E::CursorVisible(id, _) => id.clone(),
            };
            let main = self.console.as_ref().is_some_and(|c| c.id == id);
            if !main && let Some(i) = self.popouts.iter().position(|p| p.console.id == id) {
                match ev {
                    E::Attached(_, Ok(d)) => self.popouts[i].console.set_display(d),
                    E::Attached(_, Err(e)) | E::Failed(_, e) => {
                        if let Some(ui) = &mut self.ui {
                            ui.set_toast(Some(format!("The console window could not show the machine: {e}")));
                        }
                        self.popouts.remove(i);
                    }
                    E::Disconnected(_) => {
                        self.popouts.remove(i);
                    }
                    E::Frame(_) => self.popouts[i].render_cpu(false),
                    E::Live(_) | E::Cursor { .. } | E::CursorVisible(..) => {}
                }
                return;
            }
        }
        let Some(ui) = &mut self.ui else { return };
        match ev {
            #[cfg(windows)]
            E::Attached(id, r) => match (r, self.console.as_mut().filter(|c| c.id == id)) {
                (Ok(d), Some(c)) => c.set_display(d),
                (Ok(_), None) => {}
                (Err(e), _) => {
                    ui.set_toast(Some(format!("The console could not attach: {e}")));
                    ui.close_console();
                }
            },
            E::Live(id) => {
                if ui.console.as_deref() == Some(id.as_str()) {
                    ui.set_console_live(true);
                }
            }
            E::Failed(id, e) => {
                if ui.console.as_deref() == Some(id.as_str()) {
                    ui.set_toast(Some(format!("No GPU display for this machine: {e}")));
                    ui.close_console();
                }
            }
            E::Disconnected(id) => {
                if ui.console.as_deref() == Some(id.as_str()) {
                    ui.close_console();
                }
            }
            E::Frame(id) => {
                if ui.console.as_deref() == Some(id.as_str())
                    && let Some(g) = &self.gfx
                {
                    g.window.request_redraw();
                }
            }
            E::Cursor { .. } | E::CursorVisible(..) => {}
        }
    }

    /// Console mode: the UI paints into the backing store; the title row goes to the
    /// compositor, which owns the window once the first guest frame is in. Until then (and
    /// for good in a remote session, where the guest is drawn on the CPU too) the CPU path
    /// shows the backing store. Returns false outside console mode.
    #[cfg(windows)]
    fn render_console(&mut self) -> bool {
        let (Some(gfx), Some(ui), Some(c)) = (&mut self.gfx, &mut self.ui, &self.console) else { return false };
        let (w, h) = self.size;
        if w == 0 || h == 0 || (!ui.has_damage() && !c.cpu_pending()) {
            return true;
        }
        if self.backing.len() != w * h {
            self.backing = vec![0; w * h];
            ui.invalidate();
            self.console_full = true;
        }
        let painted = if ui.has_damage() {
            let p = ui.paint(&mut Canvas::new(&mut self.backing[..], w, h), false);
            let bar_h = (ui.console_bar_height().max(0) as usize).min(h);
            c.set_bar(&self.backing[..w * bar_h], w as u32, bar_h as u32);
            p
        } else {
            Vec::new()
        };
        if c.is_presenting() {
            return true;
        }
        // Remote session: the guest's changed pixels, all of them when the UI painted over its
        // area (or the backing store is new).
        let g = ui.console_guest_rect();
        let full = std::mem::take(&mut self.console_full) || painted.iter().any(|r| !r.intersect(g).is_empty());
        let t0 = Instant::now();
        let mut rects: Vec<[i32; 4]> = painted.iter().map(|r| [r.x, r.y, r.w, r.h]).collect();
        let guest = c.draw_cpu(&mut self.backing, w, h, full);
        let drew_guest = !guest.is_empty();
        rects.extend(guest);
        present_backing(&mut gfx.surface, &self.backing, w, h, &rects);
        // QORRAL_TRACE: what a guest frame costs the UI thread on the CPU path.
        if drew_guest && let Some(t) = self.cpu_trace.as_mut() {
            t.push(t0.elapsed().as_secs_f32() * 1000.0);
            if t.len() >= 300 || self.cpu_trace_t0.is_some_and(|s| s.elapsed().as_secs() >= 5) {
                let secs = self.cpu_trace_t0.map_or(0.0, |s| s.elapsed().as_secs_f32());
                t.sort_by(f32::total_cmp);
                eprintln!(
                    "qorral: console {}: CPU path, guest drawn and presented over {} frames ({:.0}/s): median {:.3} ms, p99 {:.3} ms",
                    c.id,
                    t.len(),
                    t.len() as f32 / secs.max(0.001),
                    t[t.len() / 2],
                    t[t.len() * 99 / 100]
                );
                t.clear();
                self.cpu_trace_t0 = Some(Instant::now());
            }
        }
        true
    }

    fn after_input(&mut self) {
        // `--popout NAME`: once the machine runs, its console window opens.
        if let (Some(key), Some(ui)) = (&self.popout_arg, &self.ui)
            && let Some(v) = ui.vms.iter().find(|v| {
                (v.id == *key || v.name.eq_ignore_ascii_case(key)) && matches!(v.state, RunState::Running { .. })
            })
        {
            self.popout_requests.push(v.id.clone());
            self.popout_arg = None;
        }
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
            if !self.gp.over && ui.pointer_cursor_is_hand() != self.hand {
                self.hand = ui.pointer_cursor_is_hand();
                g.window.set_cursor(if self.hand { CursorIcon::Pointer } else { CursorIcon::Default });
            }
            if ui.has_damage() {
                g.window.request_redraw();
            }
        }
    }

    fn render(&mut self) {
        #[cfg(windows)]
        if self.console.is_some() && self.render_console() {
            return;
        }
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
        if let (Some(key), Some(ui)) = (self.open.take(), &mut self.ui) {
            ui.open_when_ready(&key);
        }
        trace(self.t0, "ui built");
        self.after_input();
        self.render();
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    /// Over the guest: the guest's own pointer (or none when the guest hides it); elsewhere the
    /// UI's.
    fn update_cursor(&mut self) {
        let Some(g) = &self.gfx else { return };
        self.gp.apply(&g.window, self.hand);
    }

    /// Creates the windows for consoles that should pop out (needs the event loop).
    #[cfg(windows)]
    fn open_popouts(&mut self, el: &ActiveEventLoop) {
        for id in std::mem::take(&mut self.popout_requests) {
            if let Some(p) = self.popouts.iter().find(|p| p.console.id == id) {
                p.window.focus_window();
                continue;
            }
            let name = self
                .ui
                .as_ref()
                .and_then(|u| u.vms.iter().find(|v| v.id == id).map(|v| v.name.clone()))
                .unwrap_or_else(|| id.clone());
            let attrs = Window::default_attributes()
                .with_title(format!("{name} · Qorral"))
                .with_inner_size(LogicalSize::new(1280.0, 800.0))
                .with_min_inner_size(LogicalSize::new(320.0, 200.0));
            let window = match el.create_window(attrs) {
                Ok(w) => Rc::new(w),
                Err(e) => {
                    if let Some(ui) = &mut self.ui {
                        ui.set_toast(Some(format!("Could not open a console window: {e}")));
                    }
                    continue;
                }
            };
            let (hwnd, hinstance) = match raw_window_handle::HasWindowHandle::window_handle(&*window)
                .map(|h| h.as_raw())
            {
                Ok(raw_window_handle::RawWindowHandle::Win32(h)) => (h.hwnd.get(), h.hinstance.map_or(0, |i| i.get())),
                _ => continue,
            };
            platform::dark_title_bar(hwnd);
            let sz = window.inner_size();
            let p = self.proxy.clone();
            let notify: crate::console::Notify = Arc::new(move |ev| send(&p, UserEvent::Console(ev)));
            let remote = platform::is_remote_session();
            let cpu = if remote {
                match Context::new(window.clone()).and_then(|c| Surface::new(&c, window.clone())) {
                    Ok(s) => Some((s, Vec::new())),
                    Err(e) => {
                        if let Some(ui) = &mut self.ui {
                            ui.set_toast(Some(format!("Could not open a console window: {e}")));
                        }
                        continue;
                    }
                }
            } else {
                None
            };
            let console = crate::console::Console::open(
                &id,
                self.lib_root.clone(),
                (hwnd, hinstance),
                (sz.width, sz.height),
                [0.0, 0.0, sz.width as f32, sz.height as f32],
                remote,
                notify,
            );
            self.popouts.push(PopOut { window, console, gp: GuestPointer::default(), cpu });
        }
    }

    /// Input and window events for a console window. Returns false if `id` is not one.
    #[cfg(windows)]
    fn popout_event(&mut self, id: WindowId, ev: &WindowEvent) -> bool {
        let Some(i) = self.popouts.iter().position(|p| p.window.id() == id) else { return false };
        let mods = self.mods;
        let p = &mut self.popouts[i];
        match ev {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                let mut p = self.popouts.remove(i);
                if p.gp.captured {
                    set_capture(&p.window, &mut p.console, &mut p.gp, false);
                }
                drop(p.console); // before its window: it presents into it
            }
            WindowEvent::Resized(sz) => {
                p.console.resize((sz.width, sz.height), [0.0, 0.0, sz.width as f32, sz.height as f32]);
                p.console.redraw();
                p.render_cpu(true);
            }
            WindowEvent::Focused(false) => {
                p.console.release_keys();
                if p.gp.captured {
                    set_capture(&p.window, &mut p.console, &mut p.gp, false);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let inside = p.console.pointer(position.x, position.y);
                if inside != p.gp.over {
                    p.gp.over = inside;
                    p.gp.apply(&p.window, false);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                p.gp.over = false;
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(b) = guest_button(*button) {
                    p.console.button(b, *state == ElementState::Pressed);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                p.console.wheel(wheel_up(delta));
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if is_capture_key(mods, event) {
                    if event.state == ElementState::Pressed && !event.repeat {
                        let on = !p.gp.captured;
                        set_capture(&p.window, &mut p.console, &mut p.gp, on);
                    }
                } else {
                    use winit::platform::scancode::PhysicalKeyExtScancode;
                    if let Some(sc) = event.physical_key.to_scancode() {
                        p.console.key(sc, event.state == ElementState::Pressed);
                    }
                }
            }
            _ => {}
        }
        true
    }

    fn console_open(&self) -> bool {
        #[cfg(windows)]
        return self.console.is_some();
        #[cfg(not(windows))]
        false
    }

    /// A mouse button over the open console's guest goes to the guest.
    fn console_takes(&self, button: MouseButton, state: ElementState) -> bool {
        #[cfg(windows)]
        if let Some(c) = &self.console {
            let Some(b) = guest_button(button) else { return true };
            return c.button(b, state == ElementState::Pressed);
        }
        let _ = (button, state);
        false
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
            UserEvent::Console(crate::console::ConsoleEvent::Cursor { id, rgba, width, height, hot_x, hot_y }) => {
                let cursor = winit::window::CustomCursor::from_rgba(rgba, width, height, hot_x, hot_y)
                    .ok()
                    .map(|src| el.create_custom_cursor(src));
                #[cfg(windows)]
                if let Some(p) = self.popouts.iter_mut().find(|p| p.console.id == id) {
                    p.gp.cursor = cursor;
                    p.gp.apply(&p.window, false);
                    return;
                }
                let _ = id;
                self.gp.cursor = cursor;
                self.update_cursor();
            }
            UserEvent::Console(crate::console::ConsoleEvent::CursorVisible(id, v)) => {
                #[cfg(windows)]
                if let Some(p) = self.popouts.iter_mut().find(|p| p.console.id == id) {
                    p.gp.visible = v;
                    p.gp.apply(&p.window, false);
                    return;
                }
                let _ = id;
                self.gp.visible = v;
                self.update_cursor();
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

    /// Raw mouse motion: the captured console's relative movement (games).
    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, ev: winit::event::DeviceEvent) {
        #[cfg(windows)]
        if let winit::event::DeviceEvent::MouseMotion { delta: (dx, dy) } = ev {
            let (dx, dy) = (dx.round() as i32, dy.round() as i32);
            if dx == 0 && dy == 0 {
                return;
            }
            if self.gp.captured
                && let Some(c) = &self.console
            {
                c.mouse_by(dx, dy);
            } else if let Some(p) = self.popouts.iter().find(|p| p.gp.captured) {
                p.console.mouse_by(dx, dy);
            }
        }
        #[cfg(not(windows))]
        let _ = ev;
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        #[cfg(windows)]
        if !self.popout_requests.is_empty() {
            self.open_popouts(el);
        }
        #[cfg(not(windows))]
        let _ = el;
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, ev: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &ev {
            self.mods = m.state();
        }
        #[cfg(windows)]
        if self.gfx.as_ref().is_some_and(|g| g.window.id() != id) {
            self.popout_event(id, &ev);
            return;
        }
        #[cfg(not(windows))]
        let _ = id;
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
                    #[cfg(windows)]
                    if let Some(c) = &self.console {
                        let g = ui.console_guest_rect();
                        c.resize((sz.width, sz.height), [g.x as f32, g.y as f32, g.w as f32, g.h as f32]);
                        self.console_full = true;
                    }
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
                #[cfg(windows)]
                if !focused && let Some(c) = &mut self.console {
                    c.release_keys();
                    if self.gp.captured
                        && let Some(g) = &self.gfx
                    {
                        set_capture(&g.window, c, &mut self.gp, false);
                        if let Some(ui) = &mut self.ui {
                            ui.set_mouse_captured(false);
                        }
                    }
                }
                let remote = platform::is_remote_session();
                if let Some(ui) = &mut self.ui {
                    ui.set_active(focused);
                    if focused {
                        ui.set_remote(remote);
                    }
                }
                self.after_input();
            }
            WindowEvent::ModifiersChanged(_) => {}
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as i32, position.y as i32);
                #[cfg(windows)]
                if let Some(c) = &mut self.console {
                    let inside = c.pointer(position.x, position.y);
                    if inside != self.gp.over {
                        self.gp.over = inside;
                        self.update_cursor();
                    }
                }
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
            WindowEvent::MouseInput { state, button, .. } if self.console_takes(button, state) => {}
            WindowEvent::MouseWheel { delta, .. } => {
                #[cfg(windows)]
                if let Some(c) = &self.console {
                    c.wheel(wheel_up(&delta));
                }
                #[cfg(not(windows))]
                let _ = delta;
            }
            WindowEvent::KeyboardInput { event, .. } if self.console_open() => {
                #[cfg(windows)]
                if is_capture_key(self.mods, &event) {
                    if event.state == ElementState::Pressed
                        && !event.repeat
                        && let (Some(g), Some(c)) = (&self.gfx, &mut self.console)
                    {
                        let on = !self.gp.captured;
                        set_capture(&g.window, c, &mut self.gp, on);
                        let captured = self.gp.captured;
                        if let Some(ui) = &mut self.ui {
                            ui.set_mouse_captured(captured);
                        }
                        self.after_input();
                    }
                } else if let Some(c) = &mut self.console {
                    use winit::platform::scancode::PhysicalKeyExtScancode;
                    if let Some(sc) = event.physical_key.to_scancode() {
                        c.key(sc, event.state == ElementState::Pressed);
                    }
                }
                #[cfg(not(windows))]
                let _ = event;
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
