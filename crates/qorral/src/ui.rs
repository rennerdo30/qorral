//! Qorral's main screen: the **stage**, the **dock** and the **intent bar**.
//!
//! - Stage: the focused machine's live screen, large, in a bezel lit by its
//!   own colours; name, state and the real controls under it; a details
//!   drawer with hardware and the exact QEMU command.
//! - Dock: every machine as a live tile along the bottom, plus "New".
//! - Intent bar: plain words become actions ("fedora 16gb vulkan",
//!   "start arch"); the arrow keys and Enter run the results.
//!
//! The UI owns no I/O. It paints from data it was given, records damage, and
//! hands user intents to the window loop as [`Action`]s.

use crate::intent::{self, Draft, Intent, Verb};
use crate::theme as th;
use qorral_core::{GpuMode, OsKind};
use qorral_render::{Canvas, FontId, Rect, TextEngine, mix};

#[derive(Clone, Debug, PartialEq)]
pub enum RunState {
    Off,
    Starting,
    Running { since: u64 },
    Stopping,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Thumb {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

impl Thumb {
    /// The screen's average colour (sampled), for the stage light.
    pub fn average(&self) -> u32 {
        let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
        let step = (self.rgba.len() / 4 / 4096).max(1) * 4;
        for p in self.rgba.as_chunks::<4>().0.iter().step_by(step / 4) {
            r += p[0] as u64;
            g += p[1] as u64;
            b += p[2] as u64;
            n += 1;
        }
        if n == 0 {
            return th::ACCENT;
        }
        ((r / n) as u32) << 16 | ((g / n) as u32) << 8 | (b / n) as u32
    }
}

#[derive(Clone, Debug)]
pub struct VmView {
    pub id: String,
    pub name: String,
    pub os: OsKind,
    pub os_label: String,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub gpu: GpuMode,
    pub audio: qorral_core::AudioMode,
    pub uefi: bool,
    pub nvme: bool,
    pub forwards: usize,
    pub state: RunState,
    /// The latest guest screen (live while running, the last one when off).
    pub thumb: Option<Thumb>,
    pub command: String,
}

#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    pub accel: &'static str,
    pub qemu_version: Option<String>,
    pub qemu_found: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Start {
        id: String,
        window: bool,
    },
    Stop {
        id: String,
        force: bool,
    },
    /// Ask for an install image, then create the machine from the draft.
    Create(Draft),
    /// Show this running machine's screen in the window (its console).
    OpenConsole {
        id: String,
    },
    /// Back to the library.
    CloseConsole,
    /// Press Ctrl+Alt+Del in the guest.
    SendCtrlAltDel {
        id: String,
    },
}

/// How the window frame looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chrome {
    /// Windows 11: the top bar is the title row; Qorral draws the caption
    /// buttons and the frame keeps Snap Layouts on maximise (`frame.rs`).
    Windows11,
    /// macOS: transparent title bar, traffic lights inside the top bar.
    MacInset,
    /// The platform's own decorations (Linux for now).
    Native,
}

impl Chrome {
    pub fn for_platform() -> Chrome {
        if cfg!(windows) {
            Chrome::Windows11
        } else if cfg!(target_os = "macos") {
            Chrome::MacInset
        } else {
            Chrome::Native
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinAction {
    Minimize,
    Close,
}

/// The title row for the frame's hit testing (physical client pixels).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleGeometry {
    pub height: i32,
    pub controls: Vec<(i32, i32)>,
    pub buttons_left: i32,
    pub max_button: (i32, i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Bar,
    Result(usize),
    Screen,
    Primary,
    Secondary,
    Details,
    Dock(usize),
    DockNew,
    Suggest(usize),
    ConsoleBack,
    ConsoleCad,
    ConsoleStop,
    Guest,
}

pub struct Fonts {
    pub regular: FontId,
    pub semi: FontId,
    pub mono: FontId,
}

/// Fonts by preference: Inter where installed, the platform UI font otherwise.
pub fn load_fonts(te: &mut TextEngine) -> Option<Fonts> {
    #[cfg(windows)]
    let (r, s, m): (&[&str], &[&str], &[&str]) = (
        &[r"C:\Windows\Fonts\Inter-Regular.ttf", r"C:\Windows\Fonts\segoeui.ttf"],
        &[r"C:\Windows\Fonts\Inter-SemiBold.ttf", r"C:\Windows\Fonts\seguisb.ttf"],
        &[r"C:\Windows\Fonts\CascadiaMono.ttf", r"C:\Windows\Fonts\consola.ttf"],
    );
    #[cfg(target_os = "macos")]
    let (r, s, m): (&[&str], &[&str], &[&str]) = (
        &["/Library/Fonts/Inter-Regular.ttf", "/System/Library/Fonts/Supplemental/Arial.ttf"],
        &["/Library/Fonts/Inter-SemiBold.ttf", "/System/Library/Fonts/Supplemental/Arial Bold.ttf"],
        &["/System/Library/Fonts/Supplemental/Courier New.ttf"],
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let (r, s, m): (&[&str], &[&str], &[&str]) = (
        &["/usr/share/fonts/truetype/inter/Inter-Regular.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"],
        &["/usr/share/fonts/truetype/inter/Inter-SemiBold.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"],
        &["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"],
    );
    // Each face parses on its own thread: startup waits for the slowest one,
    // not for the sum (owner PC: ~38 ms in sequence).
    let pick = |c: &[&str]| c.iter().map(|p| p.to_string()).find(|p| std::path::Path::new(p).is_file());
    let jobs: Vec<_> = [pick(r), pick(s), pick(m)]
        .into_iter()
        .map(|p| std::thread::spawn(move || p.and_then(|p| std::fs::read(p).ok()).and_then(TextEngine::parse)))
        .collect();
    let mut faces = jobs.into_iter().map(|j| j.join().ok().flatten());
    let (fr, fsemi, fm) = (faces.next().flatten(), faces.next().flatten(), faces.next().flatten());
    let regular = te.add(fr?);
    let semi = fsemi.map_or(regular, |f| te.add(f));
    let mono = fm.map_or(regular, |f| te.add(f));
    Some(Fonts { regular, semi, mono })
}

/// Systems suggested on an empty library (factual OS names only).
pub const TILES: [(&str, &str); 5] = [
    ("ubuntu", "Ubuntu"),
    ("fedora", "Fedora"),
    ("debian", "Debian"),
    ("arch", "Arch Linux"),
    ("windows-11", "Windows 11"),
];

#[derive(Default)]
pub struct Stats {
    pub presents: u64,
    pub last_paint_ms: f64,
    pub last_damage_px: u64,
    pub first_frame_ms: Option<f64>,
}

pub struct Ui {
    te: TextEngine,
    f: Fonts,
    pub w: i32,
    pub h: i32,
    pub scale: f32,
    pub remote: bool,
    pub host: HostInfo,
    pub vms: Vec<VmView>,
    pub query: String,
    bar_focus: bool,
    results: Vec<Intent>,
    result_sel: usize,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    pub selected: Option<String>,
    pub details_open: bool,
    pub toast: Option<String>,
    pub now: u64,
    /// Stage light colour: the focused screen's average, fixed per focus so a
    /// changing screen never leaves seams in the light.
    glow: u32,
    hits: Vec<(Rect, Hit)>,
    damage: Vec<Rect>,
    actions: Vec<Action>,
    pub stats: Stats,
    pub chrome: Chrome,
    cap_hover: Option<u8>,
    cap_pressed: Option<u8>,
    max_state: (bool, bool),
    maximized: bool,
    active: bool,
    pub win_action: Option<WinAction>,
    /// The machine whose console fills the window, if any.
    pub console: Option<String>,
    /// The console shows live frames (the GPU path); until then a poster.
    pub console_live: bool,
    /// Open this machine's console as soon as it runs (it was started from here).
    open_when_running: Option<String>,
}

impl Ui {
    pub fn new(te: TextEngine, f: Fonts, w: usize, h: usize, scale: f32) -> Ui {
        let mut ui = Ui {
            te,
            f,
            w: w as i32,
            h: h as i32,
            scale,
            remote: false,
            host: HostInfo::default(),
            vms: Vec::new(),
            query: String::new(),
            bar_focus: false,
            results: Vec::new(),
            result_sel: 0,
            hover: None,
            pressed: None,
            selected: None,
            details_open: false,
            toast: None,
            now: 0,
            glow: th::ACCENT,
            hits: Vec::new(),
            damage: Vec::new(),
            actions: Vec::new(),
            stats: Stats::default(),
            chrome: Chrome::for_platform(),
            cap_hover: None,
            cap_pressed: None,
            max_state: (false, false),
            maximized: false,
            active: true,
            win_action: None,
            console: None,
            console_live: false,
            open_when_running: None,
        };
        ui.layout();
        ui.invalidate();
        ui
    }

    fn p(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }
    fn fs(&self, v: f32) -> f32 {
        v * self.scale
    }

    pub fn invalidate(&mut self) {
        self.damage.clear();
        self.damage.push(Rect::new(0, 0, self.w, self.h));
    }
    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    pub fn resize(&mut self, w: usize, h: usize, scale: f32) {
        self.w = w as i32;
        self.h = h as i32;
        self.scale = scale;
        self.relayout();
    }

    pub fn set_remote(&mut self, remote: bool) {
        if self.remote != remote {
            self.remote = remote;
            self.invalidate();
        }
    }

    pub fn set_vms(&mut self, vms: Vec<VmView>) {
        let keep = self.selected.clone().filter(|s| vms.iter().any(|v| &v.id == s));
        self.selected = keep.or_else(|| {
            // Focus the first running machine, else the first one.
            vms.iter().find(|v| matches!(v.state, RunState::Running { .. })).or(vms.first()).map(|v| v.id.clone())
        });
        self.vms = vms;
        let running =
            |id: &str, vms: &[VmView]| vms.iter().any(|v| v.id == id && matches!(v.state, RunState::Running { .. }));
        if let Some(key) = self.open_when_running.clone()
            && let Some(v) = self.vms.iter().find(|v| {
                (v.id == key || v.name.eq_ignore_ascii_case(&key)) && matches!(v.state, RunState::Running { .. })
            })
        {
            let id = v.id.clone();
            self.open_when_running = None;
            self.open_console(&id);
        }
        if let Some(id) = self.console.clone()
            && !running(&id, &self.vms)
        {
            self.close_console();
        }
        self.update_glow();
        self.refresh_results();
        self.relayout();
    }

    /// Shows a running machine's console in the window.
    pub fn open_console(&mut self, id: &str) {
        if self.console.as_deref() == Some(id) {
            return;
        }
        if self.console.is_some() {
            self.actions.push(Action::CloseConsole);
        }
        self.console = Some(id.to_string());
        self.console_live = false;
        self.select_id(id);
        self.actions.push(Action::OpenConsole { id: id.to_string() });
        self.relayout();
    }

    /// Opens this machine's console (id or name) now if it runs, else once it does.
    pub fn open_when_ready(&mut self, key: &str) {
        let Some(v) = self.vms.iter().find(|v| v.id == key || v.name.eq_ignore_ascii_case(key)) else {
            self.open_when_running = Some(key.to_string());
            return;
        };
        let id = v.id.clone();
        if matches!(v.state, RunState::Running { .. }) {
            self.open_console(&id);
        } else {
            self.open_when_running = Some(id);
        }
    }

    /// Back to the library.
    pub fn close_console(&mut self) {
        if self.console.take().is_some() {
            self.console_live = false;
            self.actions.push(Action::CloseConsole);
            self.relayout();
        }
    }

    /// The console now shows live frames (or stopped showing them).
    pub fn set_console_live(&mut self, live: bool) {
        if self.console_live != live {
            self.console_live = live;
            self.invalidate();
        }
    }

    pub fn select_id(&mut self, id: &str) {
        if self.vms.iter().any(|v| v.id == id) {
            self.selected = Some(id.to_string());
            self.update_glow();
            self.relayout();
        }
    }

    fn update_glow(&mut self) {
        self.glow = self
            .selected_vm()
            .and_then(|v| v.thumb.as_ref())
            .map(|t| mix(t.average(), th::ACCENT, 60))
            .unwrap_or(th::ACCENT);
    }

    /// A new guest screen: damages only the stage screen (when focused) and
    /// that machine's dock tile.
    pub fn set_thumb(&mut self, id: &str, thumb: Thumb) {
        let Some(i) = self.vms.iter().position(|v| v.id == id) else { return };
        let first = self.vms[i].thumb.is_none();
        let old_aspect = self.vms[i].thumb.as_ref().map(|t| (t.w, t.h));
        let new_aspect = (thumb.w, thumb.h);
        self.vms[i].thumb = Some(thumb);
        let focused = self.selected_index() == Some(i);
        if focused && (first || old_aspect != Some(new_aspect)) {
            // The light and the bezel size follow the first frame.
            self.update_glow();
            self.relayout();
            return;
        }
        if focused {
            let s = self.screen_rect();
            self.damage.push(s);
        }
        let tile = self.hits.iter().find(|(_, h)| *h == Hit::Dock(i)).map(|(r, _)| *r);
        if let Some(r) = tile {
            self.damage.push(r);
        }
    }

    pub fn set_toast(&mut self, t: Option<String>) {
        if self.toast != t {
            let old = self.toast_rect();
            self.toast = t;
            self.damage.push(old);
            let r = self.toast_rect();
            self.damage.push(r);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected.as_ref().and_then(|s| self.vms.iter().position(|v| &v.id == s))
    }
    fn selected_vm(&self) -> Option<&VmView> {
        self.selected_index().map(|i| &self.vms[i])
    }

    fn relayout(&mut self) {
        self.layout();
        self.invalidate();
    }

    // ---------- geometry ----------

    fn bar_h(&self) -> i32 {
        self.p(52.0)
    }
    fn dock_h(&self) -> i32 {
        if self.vms.is_empty() { 0 } else { self.p(138.0) }
    }
    fn drawer_w(&self) -> i32 {
        if self.details_open && self.selected_index().is_some() { self.p(400.0).min(self.w / 2) } else { 0 }
    }
    fn caption_rect(&self) -> Rect {
        if self.chrome != Chrome::Windows11 {
            return Rect::default();
        }
        let w = 3 * self.p(46.0);
        Rect::new(self.w - w, 0, w, self.bar_h() - 1)
    }
    fn mac_inset(&self) -> i32 {
        if self.chrome == Chrome::MacInset { self.p(72.0) } else { 0 }
    }
    /// The intent bar: in the title row, or centred on an empty stage.
    fn bar_rect(&self) -> Rect {
        if self.vms.is_empty() {
            let w = self.p(680.0).min(self.w - self.p(64.0));
            let h = self.p(56.0);
            Rect::new((self.w - w) / 2, self.h / 2 - self.p(40.0), w, h)
        } else {
            let w = self.p(620.0).min(self.w - self.p(560.0)).max(self.p(260.0));
            let h = self.p(36.0);
            Rect::new((self.w - w) / 2, (self.bar_h() - h) / 2, w, h)
        }
    }
    /// Where the guest's screen goes while a console is open: everything under the title row.
    pub fn console_guest_rect(&self) -> Rect {
        Rect::new(0, self.bar_h(), self.w, (self.h - self.bar_h()).max(0))
    }
    /// The console's title row height (the part drawn over the guest).
    pub fn console_bar_height(&self) -> i32 {
        self.bar_h()
    }
    /// Back, Ctrl+Alt+Del and Shut down in the console's title row.
    fn console_buttons(&self) -> (Rect, Rect, Rect) {
        let h = self.p(32.0);
        let y = (self.bar_h() - h) / 2;
        let back = Rect::new(self.mac_inset() + self.p(12.0), y, self.p(104.0), h);
        let right = if self.caption_rect().is_empty() { self.w } else { self.caption_rect().x } - self.p(14.0);
        let stop = Rect::new(right - self.p(108.0), y, self.p(108.0), h);
        let cad = Rect::new(stop.x - self.p(8.0) - self.p(132.0), y, self.p(132.0), h);
        (back, cad, stop)
    }
    fn stage_rect(&self) -> Rect {
        let y = self.bar_h();
        Rect::new(0, y, self.w - self.drawer_w(), self.h - y - self.dock_h())
    }
    /// The guest screen's bezel, fitted to the stage at the guest's aspect.
    fn bezel_rect(&self) -> Rect {
        let st = self.stage_rect();
        let info = self.p(112.0);
        let (mx, mt) = (self.p(56.0), self.p(30.0));
        let (aw, ah) = ((st.w - 2 * mx).max(1), (st.h - mt - info).max(1));
        let (gw, gh) = self.selected_vm().and_then(|v| v.thumb.as_ref()).map_or((16, 10), |t| (t.w as i32, t.h as i32));
        let pad = self.p(10.0);
        let k = ((aw - 2 * pad) as f32 / gw as f32).min((ah - 2 * pad) as f32 / gh as f32).max(0.01);
        let (w, h) = ((gw as f32 * k) as i32 + 2 * pad, (gh as f32 * k) as i32 + 2 * pad);
        Rect::new(st.x + (st.w - w) / 2, st.y + mt + (ah - h) / 2, w, h)
    }
    fn screen_rect(&self) -> Rect {
        self.bezel_rect().inset(self.p(10.0), self.p(10.0))
    }
    fn info_y(&self) -> i32 {
        self.bezel_rect().bottom() + self.p(22.0)
    }
    fn control_rects(&self) -> (Rect, Rect, Rect) {
        let b = self.bezel_rect();
        let h = self.p(40.0);
        let y = self.info_y() + self.p(6.0);
        let d = Rect::new(b.right() - h, y, h, h);
        let s = Rect::new(d.x - self.p(10.0) - self.p(176.0), y, self.p(176.0), h);
        let p = Rect::new(s.x - self.p(10.0) - self.p(132.0), y, self.p(132.0), h);
        (p, s, d)
    }
    fn dock_tile(&self, n: i32) -> Rect {
        let (tw, th, gap) = (self.p(158.0), self.p(89.0), self.p(16.0));
        let y = self.h - self.dock_h() + self.p(16.0);
        Rect::new(self.p(24.0) + n * (tw + gap), y, tw, th)
    }
    fn result_rect(&self, i: usize) -> Rect {
        let b = self.bar_rect();
        let rh = self.p(60.0);
        Rect::new(b.x, b.bottom() + self.p(8.0) + self.p(6.0) + i as i32 * rh, b.w, rh)
    }
    fn results_panel(&self) -> Rect {
        let b = self.bar_rect();
        let n = self.results.len() as i32;
        Rect::new(b.x, b.bottom() + self.p(8.0), b.w, n * self.p(60.0) + self.p(12.0))
    }
    fn results_open(&self) -> bool {
        self.bar_focus && !self.results.is_empty()
    }
    fn suggest_rects(&self) -> Vec<Rect> {
        let b = self.bar_rect();
        let (h, gap) = (self.p(40.0), self.p(10.0));
        let widths: Vec<i32> = TILES
            .iter()
            .map(|t| t.1)
            .chain(["Any ISO file…"])
            .map(|n| (n.chars().count() as f32 * self.fs(7.6) + self.fs(64.0)) as i32)
            .collect();
        let total: i32 = widths.iter().sum::<i32>() + gap * (widths.len() as i32 - 1);
        let mut x = self.w / 2 - total / 2;
        let y = b.bottom() + self.p(28.0);
        widths
            .into_iter()
            .map(|w| {
                let r = Rect::new(x, y, w, h);
                x += w + gap;
                r
            })
            .collect()
    }
    fn toast_rect(&self) -> Rect {
        let w = self.p(560.0).min(self.w - self.p(40.0));
        let bottom = self.h - self.dock_h() - self.p(18.0);
        Rect::new((self.w - w) / 2, bottom - self.p(44.0), w, self.p(44.0))
    }

    fn layout(&mut self) {
        let mut hits = Vec::new();
        if self.console.is_some() {
            let (back, cad, stop) = self.console_buttons();
            hits.push((back, Hit::ConsoleBack));
            hits.push((cad, Hit::ConsoleCad));
            hits.push((stop, Hit::ConsoleStop));
            hits.push((self.console_guest_rect(), Hit::Guest));
            self.hits = hits;
            return;
        }
        if self.results_open() {
            for i in 0..self.results.len() {
                hits.push((self.result_rect(i), Hit::Result(i)));
            }
        }
        hits.push((self.bar_rect(), Hit::Bar));
        if self.vms.is_empty() {
            if !self.results_open() {
                for (i, r) in self.suggest_rects().into_iter().enumerate() {
                    hits.push((r, Hit::Suggest(i)));
                }
            }
        } else {
            let (p, s, d) = self.control_rects();
            hits.push((p, Hit::Primary));
            hits.push((s, Hit::Secondary));
            hits.push((d, Hit::Details));
            hits.push((self.screen_rect(), Hit::Screen));
            for i in 0..self.vms.len() {
                hits.push((self.dock_tile(i as i32), Hit::Dock(i)));
            }
            hits.push((self.dock_tile(self.vms.len() as i32), Hit::DockNew));
        }
        self.hits = hits;
    }

    // ---------- frame (Windows 11 caption) ----------

    fn caption_button_at(&self, x: i32, y: i32) -> Option<u8> {
        let r = self.caption_rect();
        if r.is_empty() || !r.contains(x, y) {
            return None;
        }
        Some((((x - r.x) / (r.w / 3).max(1)) as u8).min(2))
    }

    pub fn title_geometry(&self) -> TitleGeometry {
        let c = self.caption_rect();
        let bw = c.w / 3;
        let b = self.bar_rect();
        let controls = if self.console.is_some() {
            let (back, cad, stop) = self.console_buttons();
            vec![(back.x, back.right()), (cad.x, stop.right())]
        } else if self.vms.is_empty() {
            Vec::new()
        } else {
            vec![(b.x, b.right())]
        };
        TitleGeometry {
            height: self.bar_h(),
            controls,
            buttons_left: if c.is_empty() { i32::MAX } else { c.x },
            max_button: (c.x + bw, c.x + 2 * bw),
        }
    }

    pub fn set_frame_state(&mut self, max_state: (bool, bool), maximized: bool) {
        if max_state != self.max_state || maximized != self.maximized {
            self.max_state = max_state;
            self.maximized = maximized;
            self.damage.push(self.caption_rect());
        }
    }

    pub fn set_active(&mut self, active: bool) {
        if active != self.active {
            self.active = active;
            self.damage.push(self.caption_rect());
        }
    }

    // ---------- input ----------

    fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn rect_of(&self, h: Hit) -> Option<Rect> {
        match h {
            // The screen's hover shows the big action and the bezel highlight.
            Hit::Screen => Some(self.bezel_rect()),
            _ => self.hits.iter().find(|(_, x)| *x == h).map(|(r, _)| *r),
        }
    }

    pub fn pointer_move(&mut self, x: i32, y: i32) {
        let ch = self.caption_button_at(x, y).filter(|&b| b != 1);
        if ch != self.cap_hover {
            self.cap_hover = ch;
            self.damage.push(self.caption_rect());
        }
        let h = self.hit(x, y);
        if h != self.hover {
            let rects: Vec<Rect> = [self.hover, h].into_iter().flatten().filter_map(|h| self.rect_of(h)).collect();
            self.damage.extend(rects);
            self.hover = h;
            if let Some(Hit::Result(i)) = h
                && i != self.result_sel
            {
                self.result_sel = i;
                self.damage.push(self.results_panel());
            }
        }
    }

    pub fn pointer_leave(&mut self) {
        if self.cap_hover.take().is_some() {
            self.damage.push(self.caption_rect());
        }
        if let Some(r) = self.hover.take().and_then(|h| self.rect_of(h)) {
            self.damage.push(r);
        }
    }

    pub fn pointer_cursor_is_hand(&self) -> bool {
        matches!(
            self.hover,
            Some(
                Hit::Result(_)
                    | Hit::Primary
                    | Hit::Secondary
                    | Hit::Details
                    | Hit::Dock(_)
                    | Hit::DockNew
                    | Hit::Suggest(_)
                    | Hit::Screen
                    | Hit::ConsoleBack
                    | Hit::ConsoleCad
                    | Hit::ConsoleStop
            )
        )
    }

    pub fn press(&mut self, x: i32, y: i32) {
        if let Some(b) = self.caption_button_at(x, y).filter(|&b| b != 1) {
            self.cap_pressed = Some(b);
            self.damage.push(self.caption_rect());
            return;
        }
        self.pressed = self.hit(x, y);
    }

    pub fn release(&mut self, x: i32, y: i32) {
        // A caption button acts only if the press started on it and the
        // pointer is still over it, as on Windows.
        if let Some(b) = self.cap_pressed.take() {
            if self.caption_button_at(x, y) == Some(b) {
                self.win_action = Some(if b == 0 { WinAction::Minimize } else { WinAction::Close });
            }
            self.damage.push(self.caption_rect());
            return;
        }
        let h = self.hit(x, y);
        if h.is_none() || h != self.pressed.take() {
            if h.is_none() && self.bar_focus {
                self.set_bar_focus(false);
            }
            return;
        }
        if h != Some(Hit::Bar) && !matches!(h, Some(Hit::Result(_))) && self.bar_focus {
            self.set_bar_focus(false);
        }
        match h {
            Some(Hit::Bar) => self.set_bar_focus(true),
            Some(Hit::Result(i)) => self.run_result(i),
            Some(Hit::Screen) | Some(Hit::Primary) => self.primary(),
            Some(Hit::Secondary) => self.secondary(),
            Some(Hit::Details) => {
                self.details_open = !self.details_open;
                self.relayout();
            }
            Some(Hit::Dock(i)) => {
                let id = self.vms[i].id.clone();
                self.select_id(&id);
            }
            Some(Hit::DockNew) => {
                self.query.clear();
                self.set_bar_focus(true);
            }
            Some(Hit::Suggest(i)) => {
                let preset = TILES.get(i).map(|t| t.0);
                self.actions.push(Action::Create(Draft { preset, ..Draft::default() }));
            }
            Some(Hit::ConsoleBack) => self.close_console(),
            Some(Hit::ConsoleCad) => {
                if let Some(id) = self.console.clone() {
                    self.actions.push(Action::SendCtrlAltDel { id });
                }
            }
            Some(Hit::ConsoleStop) => {
                if let Some(id) = self.console.clone() {
                    self.actions.push(Action::Stop { id, force: false });
                }
            }
            Some(Hit::Guest) | None => {}
        }
    }

    fn set_bar_focus(&mut self, on: bool) {
        if self.bar_focus != on {
            self.bar_focus = on;
            self.relayout();
        }
    }

    fn primary(&mut self) {
        let Some(v) = self.selected_vm() else { return };
        let id = v.id.clone();
        match v.state {
            RunState::Off | RunState::Failed(_) => {
                self.open_when_running = Some(id.clone());
                self.actions.push(Action::Start { id, window: false });
            }
            RunState::Running { .. } => self.open_console(&id),
            RunState::Starting | RunState::Stopping => {}
        }
    }

    fn secondary(&mut self) {
        let Some(v) = self.selected_vm() else { return };
        let id = v.id.clone();
        match v.state {
            RunState::Off | RunState::Failed(_) => self.actions.push(Action::Start { id, window: false }),
            RunState::Running { .. } => self.actions.push(Action::Stop { id, force: false }),
            RunState::Stopping => self.actions.push(Action::Stop { id, force: true }),
            RunState::Starting => {}
        }
    }

    fn refresh_results(&mut self) {
        let names: Vec<(String, bool)> =
            self.vms.iter().map(|v| (v.name.clone(), matches!(v.state, RunState::Running { .. }))).collect();
        self.results = intent::parse(&self.query, &names);
        self.result_sel = self.result_sel.min(self.results.len().saturating_sub(1));
    }

    fn run_result(&mut self, i: usize) {
        let Some(r) = self.results.get(i).cloned() else { return };
        match r {
            Intent::Create(d) => self.actions.push(Action::Create(d)),
            Intent::Machine(verb, m) => {
                let Some(v) = self.vms.get(m) else { return };
                let (id, running) = (v.id.clone(), matches!(v.state, RunState::Running { .. }));
                self.select_id(&id);
                match verb {
                    Verb::Open => {
                        if running {
                            self.open_console(&id);
                        }
                    }
                    Verb::Start => {
                        self.open_when_running = Some(id.clone());
                        self.actions.push(Action::Start { id, window: false });
                    }
                    Verb::Stop => self.actions.push(Action::Stop { id, force: false }),
                    Verb::ForceOff => self.actions.push(Action::Stop { id, force: true }),
                }
            }
        }
        self.query.clear();
        self.refresh_results();
        self.set_bar_focus(false);
        self.relayout();
    }

    /// Typed text goes to the intent bar.
    pub fn text(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        if s.is_empty() {
            return;
        }
        self.bar_focus = true;
        self.query.push_str(&s);
        self.result_sel = 0;
        self.refresh_results();
        self.relayout();
    }

    pub fn key_backspace(&mut self) {
        if self.query.pop().is_some() {
            self.refresh_results();
            self.relayout();
        }
    }

    pub fn key_escape(&mut self) {
        if !self.query.is_empty() || self.bar_focus {
            self.query.clear();
            self.refresh_results();
            self.bar_focus = false;
            self.relayout();
        } else if self.details_open {
            self.details_open = false;
            self.relayout();
        }
    }

    pub fn key_focus_search(&mut self) {
        self.set_bar_focus(true);
    }

    /// Left/right walk the dock; up/down walk the results while the bar is open.
    pub fn key_move(&mut self, d: i32) {
        if self.results_open() && d.abs() == 2 {
            let n = self.results.len() as i32;
            self.result_sel = (self.result_sel as i32 + d.signum()).rem_euclid(n) as usize;
            self.damage.push(self.results_panel());
            return;
        }
        if self.vms.is_empty() || d.abs() != 1 {
            return;
        }
        let cur = self.selected_index().unwrap_or(0) as i32;
        let n = (cur + d).clamp(0, self.vms.len() as i32 - 1) as usize;
        let id = self.vms[n].id.clone();
        self.select_id(&id);
    }

    pub fn key_enter(&mut self) {
        if self.results_open() {
            self.run_result(self.result_sel);
        } else if !self.bar_focus {
            self.primary();
        }
    }

    // ---------- paint ----------

    /// Paints every damaged rectangle and returns them (the present list).
    pub fn paint(&mut self, cv: &mut Canvas, force_full: bool) -> Vec<Rect> {
        let t = std::time::Instant::now();
        if force_full {
            self.invalidate();
        }
        let mut rects = coalesce(std::mem::take(&mut self.damage), Rect::new(0, 0, self.w, self.h));
        rects.retain(|r| !r.is_empty());
        for r in &rects {
            cv.set_clip(*r);
            self.paint_all(cv);
        }
        cv.set_clip(Rect::new(0, 0, self.w, self.h));
        self.stats.last_paint_ms = t.elapsed().as_secs_f64() * 1000.0;
        self.stats.last_damage_px = rects.iter().map(|r| r.area()).sum();
        self.stats.presents += 1;
        rects
    }

    fn paint_all(&mut self, cv: &mut Canvas) {
        cv.fill(Rect::new(0, 0, self.w, self.h), th::BG);
        if self.console.is_some() {
            self.paint_console(cv);
            self.paint_toast(cv);
            return;
        }
        if self.vms.is_empty() {
            self.paint_welcome(cv);
        } else {
            self.paint_stage(cv);
            self.paint_dock(cv);
            if self.drawer_w() > 0 {
                self.paint_drawer(cv);
            }
        }
        self.paint_bar(cv);
        if self.results_open() {
            self.paint_results(cv);
        }
        self.paint_toast(cv);
    }

    fn paint_toast(&mut self, cv: &mut Canvas) {
        if let Some(t) = self.toast.clone() {
            let r = self.toast_rect();
            cv.fill_round(r, self.fs(12.0), 0x2A1B20);
            cv.fill_circle(r.x as f32 + self.fs(20.0), r.y as f32 + r.h as f32 / 2.0, self.fs(4.0), th::FAILED);
            let (f, s) = (self.f.regular, self.fs(13.0));
            self.te.draw_in(
                cv,
                f,
                s,
                Rect::new(r.x + self.p(30.0), r.y, r.w - self.p(40.0), r.h),
                self.fs(4.0),
                &t,
                th::TEXT,
            );
        }
    }

    /// The console: a slim title row (back, machine, Ctrl+Alt+Del, shut down) over the guest.
    /// The guest area is painted only until live frames arrive (the GPU path draws it).
    fn paint_console(&mut self, cv: &mut Canvas) {
        let Some(v) = self.console.clone().and_then(|id| self.vms.iter().find(|v| v.id == id).cloned()) else {
            return;
        };
        let (reg, semi) = (self.f.regular, self.f.semi);
        let bar = Rect::new(0, 0, self.w, self.bar_h());
        cv.fill(bar, th::BG);
        cv.fill(Rect::new(0, bar.bottom() - 1, self.w, 1), th::BORDER);
        let (back, cad, stop) = self.console_buttons();
        // Back to the library: a chevron and the word.
        let bh = self.hover == Some(Hit::ConsoleBack);
        cv.round_border(
            back,
            self.fs(9.0),
            if bh { th::BORDER_STRONG } else { th::BG },
            if bh { th::CARD } else { th::BG },
        );
        let (cx, cy) = (back.x as f32 + self.fs(18.0), back.y as f32 + back.h as f32 / 2.0);
        let k = self.fs(4.5);
        cv.stroke(&[(cx + k * 0.5, cy - k), (cx - k * 0.6, cy), (cx + k * 0.5, cy + k)], self.fs(1.8), th::TEXT);
        let s13 = self.fs(13.0);
        let b = self.baseline(back, semi, s13);
        self.te.draw(cv, semi, s13, cx + self.fs(14.0), b, "Library", th::TEXT, back.w as f32);
        // The machine and its state.
        let (label, color) = self.state_style(&v.state);
        let mut x = back.right() as f32 + self.fs(20.0);
        let s14 = self.fs(14.0);
        let nb = self.baseline(bar, semi, s14);
        let room = (cad.x as f32 - x - self.fs(24.0)).max(0.0);
        x = self.te.draw(cv, semi, s14, x, nb, &v.name, th::TEXT, room * 0.6);
        x += self.fs(14.0);
        cv.fill_circle(x + self.fs(4.0), bar.h as f32 / 2.0, self.fs(4.0), color);
        x += self.fs(14.0);
        let s12 = self.fs(12.0);
        let lb = self.baseline(bar, reg, s12);
        self.te.draw(cv, reg, s12, x, lb, &label, th::MUTED, (cad.x as f32 - x - self.fs(16.0)).max(0.0));
        self.button(cv, cad, "Ctrl+Alt+Del", false, self.hover == Some(Hit::ConsoleCad), false);
        let stopping = v.state == RunState::Stopping;
        self.button(cv, stop, "Shut down", false, self.hover == Some(Hit::ConsoleStop), stopping);
        self.paint_caption(cv);

        // The guest area until live frames arrive: the last screen, dimmed, and a word.
        let g = self.console_guest_rect();
        if self.console_live || cv.clip().intersect(g).is_empty() {
            return;
        }
        cv.fill(g, 0x07080B);
        if let Some(t) = v.thumb.as_ref().filter(|t| t.w > 0 && t.h > 0) {
            let k = (g.w as f32 / t.w as f32).min(g.h as f32 / t.h as f32);
            let (w, h) = ((t.w as f32 * k) as i32, (t.h as f32 * k) as i32);
            let r = Rect::new(g.x + (g.w - w) / 2, g.y + (g.h - h) / 2, w, h);
            cv.draw_rgba_scaled(r, t.w, t.h, &t.rgba);
            cv.fill_alpha(r, th::BG, 150);
        }
        let (cx, cy) = (g.x as f32 + g.w as f32 / 2.0, g.y as f32 + g.h as f32 / 2.0);
        self.centered(cv, semi, self.fs(16.0), cx, cy, "Connecting to the display…", th::TEXT);
    }

    fn baseline(&self, r: Rect, f: FontId, size: f32) -> f32 {
        (r.y as f32 + r.h as f32 / 2.0 + self.te.ascent(f, size) * 0.36).round()
    }

    fn paint_bar(&mut self, cv: &mut Canvas) {
        let semi = self.f.semi;
        let reg = self.f.regular;
        let bar = Rect::new(0, 0, self.w, self.bar_h());
        if cv.clip().intersect(bar).is_empty() && cv.clip().intersect(self.bar_rect()).is_empty() {
            return;
        }
        // Mark: a rounded square holding a ring with a tail (the Q).
        let m = self.p(24.0);
        let mr = Rect::new(self.mac_inset() + self.p(20.0), (bar.h - m) / 2, m, m);
        cv.fill_round(mr, self.fs(7.0), th::ACCENT);
        let (cx, cy) = (mr.x as f32 + m as f32 / 2.0, mr.y as f32 + m as f32 / 2.0);
        cv.ring(cx, cy, self.fs(6.0), self.fs(2.4), th::ACCENT_TEXT);
        cv.stroke(
            &[(cx + self.fs(2.8), cy + self.fs(2.8)), (cx + self.fs(6.6), cy + self.fs(6.6))],
            self.fs(2.4),
            th::ACCENT_TEXT,
        );
        let s16 = self.fs(16.0);
        let b = (bar.h as f32 / 2.0 + self.te.ascent(semi, s16) * 0.36).round();
        self.te.draw(cv, semi, s16, mr.right() as f32 + self.fs(10.0), b, "Qorral", th::TEXT, 120.0 * self.scale);

        // Host status, right-aligned before the caption buttons.
        let right = if self.caption_rect().is_empty() { self.w } else { self.caption_rect().x };
        let mut x = right as f32 - self.fs(18.0);
        let s12 = self.fs(12.0);
        let hb = (bar.h as f32 / 2.0 + self.te.ascent(reg, s12) * 0.36).round();
        let items = [
            (
                if self.host.qemu_found {
                    format!("QEMU {}", self.host.qemu_version.clone().unwrap_or_default())
                } else {
                    "QEMU not found".to_string()
                },
                if self.host.qemu_found { th::RUNNING } else { th::FAILED },
            ),
            (self.host.accel.to_uppercase(), th::RUNNING),
        ];
        for (text, dot) in items {
            let w = self.te.measure(reg, s12, &text);
            x -= w;
            self.te.draw(cv, reg, s12, x, hb, &text, th::MUTED, w + 2.0);
            x -= self.fs(9.0);
            cv.fill_circle(x, bar.h as f32 / 2.0, self.fs(3.0), dot);
            x -= self.fs(20.0);
        }
        self.paint_caption(cv);

        // The intent bar.
        let r = self.bar_rect();
        let big = self.vms.is_empty();
        let focus = self.bar_focus;
        let hover = self.hover == Some(Hit::Bar);
        let radius = if big { self.fs(16.0) } else { self.fs(10.0) };
        if focus && !self.remote {
            // A soft accent halo while typing.
            for k in 1..=4 {
                let o = self.p(k as f32 * 1.5);
                let ring = Rect::new(r.x - o, r.y - o, r.w + 2 * o, r.h + 2 * o);
                cv.round_border(ring, radius + o as f32, mix(th::BG, th::ACCENT, 60 / k), th::BG);
            }
        }
        let border = if focus {
            th::ACCENT
        } else if hover {
            th::BORDER_STRONG
        } else {
            th::BORDER
        };
        cv.round_border(r, radius, border, th::FIELD);
        let ts = if big { self.fs(17.0) } else { self.fs(13.5) };
        let ix = r.x as f32 + if big { self.fs(26.0) } else { self.fs(18.0) };
        let iy = r.y as f32 + r.h as f32 / 2.0;
        // A prompt chevron, not a magnifier: the bar runs things, it does not only search.
        let k = if big { self.fs(6.0) } else { self.fs(4.5) };
        let col = if focus { th::ACCENT } else { th::MUTED };
        cv.stroke(&[(ix - k * 0.6, iy - k), (ix + k * 0.5, iy), (ix - k * 0.6, iy + k)], self.fs(2.0), col);
        let text_x = ix + if big { self.fs(20.0) } else { self.fs(16.0) };
        let text_r = Rect::new(text_x as i32, r.y, r.right() - text_x as i32 - self.p(80.0), r.h);
        let tb = self.baseline(r, reg, ts);
        if self.query.is_empty() {
            let hint = if big {
                "fedora 16gb vulkan, win11 named Build box, any ISO…"
            } else {
                "Run, start or create anything"
            };
            self.te.draw(cv, reg, ts, text_x, tb, hint, th::FAINT, text_r.w as f32);
            if focus {
                cv.fill(
                    Rect::new(text_x as i32 - 1, iy as i32 - (ts * 0.6) as i32, self.p(1.5).max(1), (ts * 1.2) as i32),
                    th::ACCENT,
                );
            }
        } else {
            let q = self.query.clone();
            let end = self.te.draw(cv, reg, ts, text_x, tb, &q, th::TEXT, text_r.w as f32);
            if focus {
                cv.fill(
                    Rect::new(end as i32 + 1, iy as i32 - (ts * 0.6) as i32, self.p(1.5).max(1), (ts * 1.2) as i32),
                    th::ACCENT,
                );
            }
        }
        let kc_w = self.p(48.0);
        let kc = Rect::new(r.right() - kc_w - self.p(10.0), r.y + (r.h - self.p(22.0)) / 2, kc_w, self.p(22.0));
        cv.round_border(kc, self.fs(5.0), th::BORDER_STRONG, th::FIELD);
        let label = if focus { "Esc" } else { "Ctrl K" };
        let s11 = self.fs(11.0);
        let kw = self.te.measure(reg, s11, label);
        let kb = self.baseline(kc, reg, s11);
        self.te.draw(cv, reg, s11, kc.x as f32 + (kc.w as f32 - kw) / 2.0, kb, label, th::MUTED, kw + 2.0);
    }

    fn paint_caption(&mut self, cv: &mut Canvas) {
        let r = self.caption_rect();
        if r.is_empty() || cv.clip().intersect(r).is_empty() {
            return;
        }
        let bw = r.w / 3;
        let line = self.p(1.0).max(1);
        let s = self.p(5.0);
        for i in 0..3u8 {
            let x = r.x + i as i32 * bw;
            let b = Rect::new(x, r.y, if i == 2 { r.right() - x } else { bw }, r.h);
            let (hover, pressed) = if i == 1 {
                self.max_state
            } else {
                (self.cap_hover == Some(i), self.cap_pressed == Some(i) && self.cap_hover == Some(i))
            };
            let idle = if self.active { th::TEXT } else { th::FAINT };
            let (fill, glyph) = match (i, hover, pressed) {
                (2, true, true) => (0xA51E2D, 0xFFFFFF),
                (2, true, false) => (0xC42B1C, 0xFFFFFF),
                (_, true, true) => (0x23282F, th::TEXT),
                (_, true, false) => (0x1A1E25, th::TEXT),
                _ => (th::BG, idle),
            };
            cv.fill(b, fill);
            let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
            match i {
                0 => cv.fill(Rect::new(cx - s, cy, 2 * s, line), glyph),
                1 if self.maximized => {
                    let k = self.p(2.0);
                    cv.fill(Rect::new(cx - s + k, cy - s, 2 * s - k, line), glyph);
                    cv.fill(Rect::new(cx + s - line, cy - s, line, 2 * s - k), glyph);
                    let front = Rect::new(cx - s, cy - s + k, 2 * s - k, 2 * s - k);
                    cv.fill(front, fill);
                    cv.stroke_rect(front, glyph);
                }
                1 => cv.stroke_rect(Rect::new(cx - s, cy - s, 2 * s, 2 * s), glyph),
                _ => {
                    let (fx, fy, fs) = (cx as f32, cy as f32, s as f32);
                    let w = self.fs(1.0).max(1.0);
                    cv.stroke(&[(fx - fs, fy - fs), (fx + fs, fy + fs)], w, glyph);
                    cv.stroke(&[(fx - fs, fy + fs), (fx + fs, fy - fs)], w, glyph);
                }
            }
        }
    }

    fn state_style(&self, s: &RunState) -> (String, u32) {
        match s {
            RunState::Off => ("Off".into(), th::OFF),
            RunState::Starting => ("Starting".into(), th::BUSY),
            RunState::Stopping => ("Shutting down".into(), th::BUSY),
            RunState::Failed(_) => ("Didn't start".into(), th::FAILED),
            RunState::Running { since } => {
                (format!("Running · {}", uptime(self.now.saturating_sub(*since))), th::RUNNING)
            }
        }
    }

    fn paint_stage(&mut self, cv: &mut Canvas) {
        let Some(v) = self.selected_vm().cloned() else { return };
        let (reg, semi) = (self.f.regular, self.f.semi);
        let bz = self.bezel_rect();
        let sc = self.screen_rect();
        let running = matches!(v.state, RunState::Running { .. });
        // Stage light: the screen's own colour spills onto the stage
        // (local mode; remote mode keeps solid fills).
        if !self.remote {
            let tint = if running { self.glow } else { mix(self.glow, th::BG, 140) };
            let steps = 16;
            for k in (1..=steps).rev() {
                let o = self.p(k as f32 * 5.0);
                let a = ((steps - k + 1) * (steps - k + 1)) as u32 * 30 / (steps * steps) as u32;
                let rr = Rect::new(bz.x - o, bz.y - o, bz.w + 2 * o, bz.h + 2 * o);
                // Only the ring between this step and the next: no double blending.
                let inner =
                    Rect::new(rr.x + self.p(5.0), rr.y + self.p(5.0), rr.w - 2 * self.p(5.0), rr.h - 2 * self.p(5.0));
                for band in ring_bands(rr, inner) {
                    cv.fill_alpha(band, tint, a);
                }
            }
        }
        let hover = self.hover == Some(Hit::Screen);
        let bezel_edge = if hover { mix(th::BORDER_STRONG, self.glow, 120) } else { mix(th::BORDER, self.glow, 50) };
        cv.round_border(bz, self.fs(16.0), bezel_edge, 0x07080B);
        self.paint_screen(cv, sc, &v, hover);

        // Name, state and system under the screen.
        let y = self.info_y() as f32;
        let (label, color) = self.state_style(&v.state);
        let (pr, sr, dr) = self.control_rects();
        let name_w = (pr.x - bz.x) as f32 - self.fs(24.0);
        self.te.draw(cv, semi, self.fs(24.0), bz.x as f32 + self.fs(4.0), y + self.fs(24.0), &v.name, th::TEXT, name_w);
        let ly = y + self.fs(50.0);
        let mut x = bz.x as f32 + self.fs(4.0);
        cv.fill_circle(x + self.fs(4.0), ly - self.fs(4.5), self.fs(4.0), color);
        x += self.fs(14.0);
        x = self.te.draw(cv, semi, self.fs(13.0), x, ly, &label, mix(color, th::TEXT, 120), name_w);
        let mem = gib(v.memory_mib);
        let specs = format!("   {}  ·  {} vCPU  ·  {mem}  ·  {}", v.os_label, v.cpus, v.gpu.label());
        self.te.draw(cv, reg, self.fs(13.0), x, ly, &specs, th::MUTED, (pr.x as f32 - x - self.fs(16.0)).max(0.0));

        let busy = matches!(v.state, RunState::Starting | RunState::Stopping);
        let (pl, sl) = if running {
            ("Open", "Shut down")
        } else if v.state == RunState::Stopping {
            ("Open", "Force off")
        } else {
            ("Start", "Start in background")
        };
        self.button(cv, pr, pl, true, self.hover == Some(Hit::Primary), busy);
        self.button(cv, sr, sl, false, self.hover == Some(Hit::Secondary), v.state == RunState::Starting);
        // Details toggle: three lines.
        let dh = self.hover == Some(Hit::Details) || self.details_open;
        cv.round_border(
            dr,
            self.fs(10.0),
            if dh { th::BORDER_STRONG } else { th::BORDER },
            if self.details_open { th::CARD_HOVER } else { th::CARD },
        );
        let (cx, cy) = (dr.x as f32 + dr.w as f32 / 2.0, dr.y as f32 + dr.h as f32 / 2.0);
        for d in [-5.0f32, 0.0, 5.0] {
            let w = if d == 0.0 { 5.0 } else { 7.0 };
            cv.stroke(
                &[(cx - self.fs(7.0), cy + self.fs(d)), (cx + self.fs(w), cy + self.fs(d))],
                self.fs(1.6),
                th::TEXT,
            );
        }
    }

    /// The guest screen: live, the last one seen, or a poster; with the big
    /// action on hover (or always when off).
    fn paint_screen(&mut self, cv: &mut Canvas, sc: Rect, v: &VmView, hover: bool) {
        let (reg, semi) = (self.f.regular, self.f.semi);
        let running = matches!(v.state, RunState::Running { .. });
        let old = cv.set_clip(cv.clip().intersect(sc));
        match &v.thumb {
            Some(img) if img.w > 0 && img.h > 0 => {
                cv.fill(sc, th::SCREEN);
                cv.draw_rgba_scaled(sc, img.w, img.h, &img.rgba);
                if !running && !self.remote {
                    cv.fill_alpha(sc, th::BG, 165);
                }
            }
            _ => {
                let tint = th::os_tint(&v.os_label);
                cv.fill(sc, mix(th::SCREEN, tint, 22));
            }
        }
        cv.set_clip(old);
        let (cx, cy) = (sc.x as f32 + sc.w as f32 / 2.0, sc.y as f32 + sc.h as f32 / 2.0);
        match &v.state {
            RunState::Running { .. } => {
                if v.thumb.is_none() {
                    self.centered(cv, reg, self.fs(14.0), cx, cy, "Waiting for the first frame…", th::MUTED);
                }
                // LIVE badge.
                let s = self.fs(11.0);
                let w = self.te.measure(semi, s, "LIVE");
                let tag = Rect::new(sc.x + self.p(14.0), sc.y + self.p(14.0), (w + self.fs(26.0)) as i32, self.p(22.0));
                cv.fill_round(tag, self.fs(6.0), 0x0B0D11);
                cv.fill_circle(
                    tag.x as f32 + self.fs(10.0),
                    tag.y as f32 + tag.h as f32 / 2.0,
                    self.fs(3.2),
                    th::RUNNING,
                );
                let b = self.baseline(tag, semi, s);
                self.te.draw(cv, semi, s, tag.x as f32 + self.fs(18.0), b, "LIVE", th::TEXT, w + 2.0);
                if hover {
                    self.big_action(cv, cx, cy, false, "Open");
                }
            }
            RunState::Starting => {
                self.centered(cv, semi, self.fs(18.0), cx, cy, "Starting…", th::TEXT);
            }
            RunState::Stopping => {
                self.centered(cv, semi, self.fs(18.0), cx, cy, "Shutting down…", th::TEXT);
            }
            RunState::Failed(msg) => {
                self.centered(cv, semi, self.fs(20.0), cx, cy - self.fs(36.0), "This machine didn't start", th::TEXT);
                let s13 = self.fs(13.0);
                let lines = wrap(&mut self.te, reg, s13, msg, sc.w as f32 * 0.7, 3);
                for (n, l) in lines.iter().enumerate() {
                    self.centered(
                        cv,
                        reg,
                        self.fs(13.0),
                        cx,
                        cy - self.fs(8.0) + n as f32 * self.fs(19.0),
                        l,
                        mix(th::FAILED, th::TEXT, 90),
                    );
                }
                self.big_action(cv, cx, cy + self.fs(72.0), hover, "Try again");
            }
            RunState::Off => {
                if v.thumb.is_none() {
                    let tint = th::os_tint(&v.os_label);
                    let rad = self.fs(38.0);
                    cv.fill_circle(cx, cy - self.fs(52.0), rad, mix(th::SCREEN, tint, 150));
                    let ini = initials(&v.os_label);
                    self.centered(cv, semi, self.fs(26.0), cx, cy - self.fs(52.0), &ini, 0xFFFFFF);
                    self.centered(cv, reg, self.fs(13.0), cx, cy + self.fs(4.0), "Never started", th::MUTED);
                    self.big_action(cv, cx, cy + self.fs(62.0), hover, "Start");
                } else {
                    self.big_action(cv, cx, cy, hover, "Start");
                }
            }
        }
    }

    /// A round play/power button with a label under it.
    fn big_action(&mut self, cv: &mut Canvas, cx: f32, cy: f32, hot: bool, label: &str) {
        let r = self.fs(34.0);
        let fill = if hot { th::ACCENT } else { 0xF1F3F8 };
        let icon = if hot { th::ACCENT_TEXT } else { 0x0E1117 };
        cv.fill_circle(cx, cy, r, fill);
        if label == "Shut down" {
            cv.ring(cx, cy + self.fs(1.5), self.fs(11.0), self.fs(2.8), icon);
            cv.fill(
                Rect::new((cx - self.fs(4.0)) as i32, (cy - self.fs(16.0)) as i32, self.p(8.0), self.p(10.0)),
                fill,
            );
            cv.stroke(&[(cx, cy - self.fs(13.5)), (cx, cy - self.fs(2.0))], self.fs(2.8), icon);
        } else {
            let k = self.fs(13.0);
            cv.fill_triangle((cx - k * 0.62, cy - k), (cx - k * 0.62, cy + k), (cx + k * 0.95, cy), icon);
        }
        let semi = self.f.semi;
        let s = self.fs(13.0);
        let w = self.te.measure(semi, s, label);
        if !self.remote {
            let pill = Rect::new(
                (cx - w / 2.0 - self.fs(12.0)) as i32,
                (cy + r + self.fs(12.0)) as i32,
                (w + self.fs(24.0)) as i32,
                self.p(26.0),
            );
            cv.fill_round(pill, self.fs(13.0), 0x0B0D11);
        }
        self.te.draw(cv, semi, s, cx - w / 2.0, cy + r + self.fs(30.0), label, th::TEXT, w + 2.0);
    }

    #[allow(clippy::too_many_arguments)]
    fn centered(&mut self, cv: &mut Canvas, f: FontId, s: f32, cx: f32, cy: f32, text: &str, col: u32) {
        let w = self.te.measure(f, s, text);
        let b = (cy + self.te.ascent(f, s) * 0.36).round();
        self.te.draw(cv, f, s, cx - w / 2.0, b, text, col, w + 2.0);
    }

    fn paint_dock(&mut self, cv: &mut Canvas) {
        let dock = Rect::new(0, self.h - self.dock_h(), self.w, self.dock_h());
        if cv.clip().intersect(dock).is_empty() {
            return;
        }
        cv.fill(dock, th::BAR);
        cv.fill(Rect::new(0, dock.y, self.w, 1), th::BORDER);
        let (reg, semi) = (self.f.regular, self.f.semi);
        let sel = self.selected_index();
        for i in 0..self.vms.len() {
            let t = self.dock_tile(i as i32);
            if cv.clip().intersect(Rect::new(t.x, t.y, t.w, dock.bottom() - t.y)).is_empty() {
                continue;
            }
            let v = self.vms[i].clone();
            let selected = sel == Some(i);
            let hover = self.hover == Some(Hit::Dock(i));
            // Tile screen.
            let old = cv.set_clip(cv.clip().intersect(t));
            match &v.thumb {
                Some(img) => {
                    cv.fill(t, th::SCREEN);
                    cv.draw_rgba_scaled(t, img.w, img.h, &img.rgba);
                    if !matches!(v.state, RunState::Running { .. }) && !self.remote {
                        cv.fill_alpha(t, th::BG, 150);
                    }
                }
                None => {
                    let tint = th::os_tint(&v.os_label);
                    cv.fill(t, mix(th::SCREEN, tint, 30));
                    let ini = initials(&v.os_label);
                    let (cx, cy) = (t.x as f32 + t.w as f32 / 2.0, t.y as f32 + t.h as f32 / 2.0);
                    cv.fill_circle(cx, cy, self.fs(18.0), mix(th::SCREEN, tint, 150));
                    self.centered(cv, semi, self.fs(13.0), cx, cy, &ini, 0xFFFFFF);
                }
            }
            cv.set_clip(old);
            let edge = if selected {
                th::ACCENT
            } else if hover {
                th::BORDER_STRONG
            } else {
                th::BORDER
            };
            cv.stroke_rect(t, edge);
            if selected {
                cv.stroke_rect(t.inset(1, 1), edge);
            }
            // State dot, ringed so it reads on any screen.
            let (label, color) = self.state_style(&v.state);
            let (dx, dy) = (t.right() as f32 - self.fs(11.0), t.y as f32 + self.fs(11.0));
            cv.fill_circle(dx, dy, self.fs(6.0), 0x0B0D11);
            cv.fill_circle(dx, dy, self.fs(4.0), color);
            let _ = label;
            let nb = t.bottom() as f32 + self.fs(20.0);
            let col = if selected { th::TEXT } else { th::MUTED };
            let f = if selected { semi } else { reg };
            self.te.draw(cv, f, self.fs(12.5), t.x as f32 + self.fs(2.0), nb, &v.name, col, t.w as f32 - self.fs(4.0));
        }
        // New.
        let t = self.dock_tile(self.vms.len() as i32);
        let hover = self.hover == Some(Hit::DockNew);
        dashed_rect(cv, t, if hover { th::ACCENT } else { th::BORDER_STRONG }, self.scale);
        let (cx, cy) = (t.x as f32 + t.w as f32 / 2.0, t.y as f32 + t.h as f32 / 2.0);
        let a = self.fs(9.0);
        let col = if hover { th::ACCENT } else { th::MUTED };
        cv.stroke(&[(cx - a, cy), (cx + a, cy)], self.fs(2.0), col);
        cv.stroke(&[(cx, cy - a), (cx, cy + a)], self.fs(2.0), col);
        self.te.draw(
            cv,
            reg,
            self.fs(12.5),
            t.x as f32 + self.fs(2.0),
            t.bottom() as f32 + self.fs(20.0),
            "New machine",
            th::MUTED,
            t.w as f32,
        );
    }

    fn paint_drawer(&mut self, cv: &mut Canvas) {
        let Some(v) = self.selected_vm().cloned() else { return };
        let (reg, semi, mono) = (self.f.regular, self.f.semi, self.f.mono);
        let dw = self.drawer_w();
        let r = Rect::new(self.w - dw, self.bar_h(), dw, self.h - self.bar_h() - self.dock_h());
        cv.fill(r, th::PANEL);
        cv.fill(Rect::new(r.x, r.y, 1, r.h), th::BORDER);
        let x = r.x as f32 + self.fs(24.0);
        let w = r.w as f32 - self.fs(48.0);
        let mut y = r.y as f32 + self.fs(38.0);
        self.te.draw(cv, semi, self.fs(12.0), x, y, "HARDWARE", th::FAINT, w);
        y += self.fs(8.0);
        let rows: Vec<(&str, String)> = vec![
            ("System", v.os_label.clone()),
            ("Processors", format!("{} virtual CPUs", v.cpus)),
            ("Memory", gib(v.memory_mib)),
            ("Disk", format!("{} GB · {}", v.disk_gib, if v.nvme { "NVMe" } else { "virtio" })),
            ("Graphics", v.gpu.label().to_string()),
            ("Sound", v.audio.label().to_string()),
            ("Firmware", if v.uefi { "UEFI".into() } else { "BIOS".into() }),
            ("Network", if v.forwards == 0 { "NAT".into() } else { format!("NAT · {} forwarded", v.forwards) }),
        ];
        for (k, val) in rows {
            y += self.fs(30.0);
            cv.fill(Rect::new(x as i32, (y - self.fs(20.0)) as i32, w as i32, 1), th::BORDER);
            self.te.draw(cv, reg, self.fs(13.0), x, y, k, th::MUTED, self.fs(110.0));
            self.te.draw(cv, reg, self.fs(13.0), x + self.fs(110.0), y, &val, th::TEXT, w - self.fs(110.0));
        }
        y += self.fs(44.0);
        self.te.draw(cv, semi, self.fs(12.0), x, y, "QEMU COMMAND", th::FAINT, w);
        let hint = format!("qorral info {}", v.id);
        let hw = self.te.measure(mono, self.fs(11.5), &hint);
        self.te.draw(cv, mono, self.fs(11.5), x + w - hw, y, &hint, th::ACCENT, hw + 2.0);
        y += self.fs(10.0);
        let s = self.fs(11.5);
        let cw = self.te.measure(mono, s, "m").max(1.0);
        let per = ((w - self.fs(24.0)) / cw).floor().max(10.0) as usize;
        let chars: Vec<char> = v.command.chars().collect();
        let max_h = (r.bottom() as f32 - y - self.fs(24.0)).max(0.0);
        let need = chars.len().div_ceil(per) as f32 * self.fs(17.0) + self.fs(22.0);
        let box_h = need.min(max_h);
        if box_h > self.fs(40.0) {
            let bx = Rect::new(x as i32, y as i32, w as i32, box_h as i32);
            cv.round_border(bx, self.fs(8.0), th::BORDER, th::BG);
            let lines = ((box_h - self.fs(8.0)) / self.fs(17.0)) as usize;
            let mut ly = y + self.fs(22.0);
            for (n, chunk) in chars.chunks(per).take(lines).enumerate() {
                let line: String = chunk.iter().collect();
                self.te.draw(cv, mono, s, x + self.fs(12.0), ly, &line, if n == 0 { th::TEXT } else { th::MUTED }, w);
                ly += self.fs(17.0);
            }
        }
    }

    fn paint_results(&mut self, cv: &mut Canvas) {
        let panel = self.results_panel();
        if cv.clip().intersect(panel.inset(-self.p(12.0), -self.p(12.0))).is_empty() {
            return;
        }
        let (reg, semi) = (self.f.regular, self.f.semi);
        if !self.remote {
            for k in 1..=6 {
                let o = self.p(k as f32 * 2.0);
                let sh = Rect::new(panel.x - o, panel.y - o + self.p(6.0), panel.w + 2 * o, panel.h + 2 * o);
                for band in ring_bands(
                    sh,
                    Rect::new(sh.x + self.p(2.0), sh.y + self.p(2.0), sh.w - self.p(4.0), sh.h - self.p(4.0)),
                ) {
                    cv.fill_alpha(band, 0x000000, (7 - k) as u32 * 14);
                }
            }
        }
        cv.round_border(panel, self.fs(14.0), th::BORDER_STRONG, th::PANEL);
        let results = self.results.clone();
        for (i, res) in results.iter().enumerate() {
            let r = self.result_rect(i);
            let sel = i == self.result_sel;
            if sel {
                cv.fill_round(r.inset(self.p(6.0), self.p(2.0)), self.fs(10.0), th::CARD_HOVER);
                cv.fill_round(
                    Rect::new(r.x + self.p(6.0), r.y + self.p(14.0), self.p(3.0), r.h - self.p(28.0)),
                    self.fs(1.5),
                    th::ACCENT,
                );
            }
            let icon = Rect::new(r.x + self.p(18.0), r.y + (r.h - self.p(36.0)) / 2, self.p(56.0), self.p(36.0));
            let tx = icon.right() as f32 + self.fs(14.0);
            let (title, sub, tint, ini, thumb) = match res {
                Intent::Create(d) => {
                    let (name, cpus, mem, disk, gpu) = d.resolved();
                    let sys = d.preset.and_then(crate::model::preset).map_or("Any system", |p| p.label);
                    (
                        format!("Create “{name}”"),
                        format!("{sys}  ·  {mem} GB memory  ·  {cpus} CPUs  ·  {disk} GB disk  ·  {}", gpu.label()),
                        th::os_tint(sys),
                        if d.preset.is_some() { initials(sys) } else { "+".into() },
                        None,
                    )
                }
                Intent::Machine(verb, m) => {
                    let v = &self.vms[*m];
                    let word = match verb {
                        Verb::Open => "Open",
                        Verb::Start => "Start",
                        Verb::Stop => "Shut down",
                        Verb::ForceOff => "Force off",
                    };
                    let (st, _) = self.state_style(&v.state);
                    (
                        format!("{word} “{}”", v.name),
                        format!("{st}  ·  {}", v.os_label),
                        th::os_tint(&v.os_label),
                        initials(&v.os_label),
                        v.thumb.clone(),
                    )
                }
            };
            match thumb {
                Some(t) => {
                    cv.fill(icon, th::SCREEN);
                    cv.draw_rgba_scaled(icon, t.w, t.h, &t.rgba);
                    cv.stroke_rect(icon, th::BORDER);
                }
                None => {
                    cv.fill_round(icon, self.fs(8.0), mix(th::PANEL, tint, 120));
                    let (cx, cy) = (icon.x as f32 + icon.w as f32 / 2.0, icon.y as f32 + icon.h as f32 / 2.0);
                    self.centered(cv, semi, self.fs(15.0), cx, cy, &ini, 0xFFFFFF);
                }
            }
            let hint_w = if sel { self.fs(130.0) } else { 0.0 };
            let avail = r.right() as f32 - tx - self.fs(18.0) - hint_w;
            self.te.draw(cv, semi, self.fs(14.5), tx, r.y as f32 + self.fs(26.0), &title, th::TEXT, avail);
            self.te.draw(cv, reg, self.fs(12.5), tx, r.y as f32 + self.fs(45.0), &sub, th::MUTED, avail);
            if sel {
                let hint = if matches!(res, Intent::Create(_)) { "Choose image  ⏎" } else { "⏎" };
                let s = self.fs(12.0);
                let hw = self.te.measure(reg, s, hint);
                self.te.draw(
                    cv,
                    reg,
                    s,
                    r.right() as f32 - self.fs(20.0) - hw,
                    r.y as f32 + r.h as f32 / 2.0 + self.fs(4.0),
                    hint,
                    th::ACCENT,
                    hw + 2.0,
                );
            }
        }
    }

    fn paint_welcome(&mut self, cv: &mut Canvas) {
        let (reg, semi) = (self.f.regular, self.f.semi);
        let b = self.bar_rect();
        let cx = self.w as f32 / 2.0;
        if !self.remote {
            // A wide, low light behind the prompt.
            let steps = 14;
            for k in (1..=steps).rev() {
                let o = self.p(k as f32 * 9.0);
                let rr = Rect::new(b.x - o * 2, b.y - o, b.w + 4 * o, b.h + 2 * o);
                let inner =
                    Rect::new(rr.x + self.p(18.0), rr.y + self.p(9.0), rr.w - self.p(36.0), rr.h - self.p(18.0));
                let a = ((steps - k + 1) * (steps - k + 1)) as u32 * 22 / (steps * steps) as u32;
                for band in ring_bands(rr, inner) {
                    cv.fill_alpha(band, th::ACCENT, a);
                }
            }
        }
        self.centered(cv, semi, self.fs(34.0), cx, b.y as f32 - self.fs(70.0), "What do you want to run?", th::TEXT);
        self.centered(
            cv,
            reg,
            self.fs(14.5),
            cx,
            b.y as f32 - self.fs(34.0),
            "Type it the way you'd say it. Qorral sets up the machine.",
            th::MUTED,
        );
        if self.results_open() {
            return;
        }
        for (i, r) in self.suggest_rects().into_iter().enumerate() {
            let hover = self.hover == Some(Hit::Suggest(i));
            let (name, tint) = match TILES.get(i) {
                Some(t) => (t.1.to_string(), th::os_tint(t.1)),
                None => ("Any ISO file…".to_string(), th::ACCENT),
            };
            cv.round_border(
                r,
                r.h as f32 / 2.0,
                if hover { mix(th::BORDER_STRONG, tint, 120) } else { th::BORDER },
                if hover { th::CARD_HOVER } else { th::CARD },
            );
            let (dx, dy) = (r.x as f32 + self.fs(20.0), r.y as f32 + r.h as f32 / 2.0);
            cv.fill_circle(dx, dy, self.fs(11.0), mix(th::CARD, tint, 190));
            let ini = if i < TILES.len() { initials(&name).chars().take(1).collect() } else { "+".to_string() };
            self.centered(cv, semi, self.fs(11.0), dx, dy, &ini, 0xFFFFFF);
            let s = self.fs(13.0);
            let bl = self.baseline(r, semi, s);
            self.te.draw(cv, semi, s, dx + self.fs(18.0), bl, &name, th::TEXT, r.w as f32);
        }
        let foot = "From a terminal:  qorral create my-vm --iso image.iso";
        let fy = self.suggest_rects().first().map_or(b.bottom(), |r| r.bottom()) as f32 + self.fs(56.0);
        self.centered(cv, self.f.mono, self.fs(12.0), cx, fy, foot, th::FAINT);
    }

    fn button(&mut self, cv: &mut Canvas, r: Rect, label: &str, primary: bool, hover: bool, busy: bool) {
        let semi = self.f.semi;
        let (bg, fg) = match (primary, busy) {
            (_, true) => (th::CARD, th::FAINT),
            (true, _) => (if hover { mix(th::ACCENT, 0xFFFFFF, 30) } else { th::ACCENT }, th::ACCENT_TEXT),
            (false, _) => (if hover { th::CARD_HOVER } else { th::CARD }, th::TEXT),
        };
        if primary || busy {
            cv.fill_round(r, self.fs(10.0), bg);
        } else {
            cv.round_border(r, self.fs(10.0), if hover { th::BORDER_STRONG } else { th::BORDER }, bg);
        }
        let s = self.fs(13.5);
        let w = self.te.measure(semi, s, label);
        let b = self.baseline(r, semi, s);
        self.te.draw(cv, semi, s, r.x as f32 + (r.w as f32 - w) / 2.0, b, label, fg, r.w as f32);
    }
}

/// The four bands of `outer` that lie outside `inner`.
fn ring_bands(outer: Rect, inner: Rect) -> [Rect; 4] {
    [
        Rect::new(outer.x, outer.y, outer.w, inner.y - outer.y),
        Rect::new(outer.x, inner.bottom(), outer.w, outer.bottom() - inner.bottom()),
        Rect::new(outer.x, inner.y, inner.x - outer.x, inner.h),
        Rect::new(inner.right(), inner.y, outer.right() - inner.right(), inner.h),
    ]
}

fn dashed_rect(cv: &mut Canvas, r: Rect, color: u32, scale: f32) {
    let (dash, gap) = ((6.0 * scale) as i32, (5.0 * scale) as i32);
    let mut x = r.x;
    while x < r.right() {
        let w = dash.min(r.right() - x);
        cv.fill(Rect::new(x, r.y, w, 1), color);
        cv.fill(Rect::new(x, r.bottom() - 1, w, 1), color);
        x += dash + gap;
    }
    let mut y = r.y;
    while y < r.bottom() {
        let h = dash.min(r.bottom() - y);
        cv.fill(Rect::new(r.x, y, 1, h), color);
        cv.fill(Rect::new(r.right() - 1, y, 1, h), color);
        y += dash + gap;
    }
}

/// Greedy word wrap into at most `max` lines (the last one ellipsised by draw).
fn wrap(te: &mut TextEngine, f: FontId, s: f32, text: &str, width: f32, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if te.measure(f, s, &cand) > width && !cur.is_empty() {
            lines.push(std::mem::replace(&mut cur, word.to_string()));
            if lines.len() == max {
                return lines;
            }
        } else {
            cur = cand;
        }
    }
    if !cur.is_empty() && lines.len() < max {
        lines.push(cur);
    }
    lines
}

fn gib(mib: u32) -> String {
    if mib.is_multiple_of(1024) { format!("{} GB", mib / 1024) } else { format!("{mib} MB") }
}

/// Merges damage: overlapping rectangles join, and many small ones become
/// their bounding box (a few presents beat hundreds).
fn coalesce(mut rects: Vec<Rect>, bounds: Rect) -> Vec<Rect> {
    rects = rects.into_iter().map(|r| r.intersect(bounds)).filter(|r| !r.is_empty()).collect();
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                if !rects[i].intersect(rects[j]).is_empty() {
                    let (a, b) = (rects[i], rects[j]);
                    let x = a.x.min(b.x);
                    let y = a.y.min(b.y);
                    rects[i] = Rect::new(x, y, a.right().max(b.right()) - x, a.bottom().max(b.bottom()) - y);
                    rects.swap_remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }
    if rects.len() > 16 {
        let x = rects.iter().map(|r| r.x).min().unwrap_or(0);
        let y = rects.iter().map(|r| r.y).min().unwrap_or(0);
        let r = rects.iter().map(|r| r.right()).max().unwrap_or(0);
        let b = rects.iter().map(|r| r.bottom()).max().unwrap_or(0);
        rects = vec![Rect::new(x, y, r - x, b - y)];
    }
    rects
}

pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    match words.as_slice() {
        [] => "?".into(),
        [one] => {
            let mut c = one.chars();
            c.next().map(|f| f.to_uppercase().chain(c.take(1)).collect()).unwrap_or_default()
        }
        [a, b, ..] => {
            let second =
                if b.chars().all(|c| c.is_ascii_digit()) { b.to_string() } else { b.chars().take(1).collect() };
            format!("{}{}", a.chars().next().unwrap_or('?'), second).chars().take(3).collect()
        }
    }
}

pub fn uptime(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
        _ => format!("{} d {} h", secs / 86400, secs % 86400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_joins_overlaps_and_caps_count() {
        let b = Rect::new(0, 0, 1000, 1000);
        let r = coalesce(vec![Rect::new(0, 0, 10, 10), Rect::new(5, 5, 10, 10), Rect::new(100, 100, 5, 5)], b);
        assert_eq!(r.len(), 2);
        let many: Vec<Rect> = (0..40).map(|i| Rect::new(i * 20, 0, 10, 10)).collect();
        assert_eq!(coalesce(many, b).len(), 1);
        assert!(coalesce(vec![Rect::new(-50, -50, 10, 10)], b).is_empty(), "off-screen damage is dropped");
    }

    #[test]
    fn ring_bands_cover_exactly_the_ring() {
        let outer = Rect::new(0, 0, 10, 10);
        let inner = Rect::new(2, 2, 6, 6);
        let area: u64 = ring_bands(outer, inner).iter().map(|r| r.area()).sum();
        assert_eq!(area, outer.area() - inner.area());
    }

    #[test]
    fn initials_and_uptime_read_well() {
        assert_eq!(initials("Windows 11"), "W11");
        assert_eq!(initials("Arch Linux"), "AL");
        assert_eq!(initials("fedora"), "Fe");
        assert_eq!(uptime(30), "just now");
        assert_eq!(uptime(3 * 3600 + 120), "3 h 2 min");
    }
}
