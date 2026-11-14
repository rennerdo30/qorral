//! The library screen: every machine at a glance (live screens, state,
//! hardware), one search field for machines and new installs, and a detail
//! pane for the selected machine.
//!
//! The UI owns no I/O. It paints from data it was given, records damage, and
//! hands user intents to the window loop as [`Action`]s.

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

#[derive(Clone, Debug)]
pub struct VmView {
    pub id: String,
    pub name: String,
    pub os: OsKind,
    /// "Fedora 43 Workstation", or the OS family when unknown.
    pub os_label: String,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub gpu: GpuMode,
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
    /// Pick an install medium and create a machine; the preset fills in defaults.
    NewFromIso {
        preset: Option<&'static str>,
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
    Search,
    New,
    Card(usize),
    CardPrimary(usize),
    Primary,
    Secondary,
    Tile(usize),
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

/// The OS tiles shown when the library is empty (factual OS names only).
pub const TILES: [(&str, &str, &str); 5] = [
    ("ubuntu", "Ubuntu", "Desktop"),
    ("fedora", "Fedora", "Workstation"),
    ("debian", "Debian", "Stable"),
    ("arch", "Arch Linux", "Rolling"),
    ("windows-11", "Windows 11", "Tuned for setup"),
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
    search_focus: bool,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    pub selected: Option<String>,
    pub toast: Option<String>,
    pub now: u64,
    hits: Vec<(Rect, Hit)>,
    damage: Vec<Rect>,
    actions: Vec<Action>,
    pub stats: Stats,
    pub chrome: Chrome,
    cap_hover: Option<u8>,
    cap_pressed: Option<u8>,
    /// Maximise button (hovered, pressed), reported by the frame.
    max_state: (bool, bool),
    maximized: bool,
    active: bool,
    pub win_action: Option<WinAction>,
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
            search_focus: false,
            hover: None,
            pressed: None,
            selected: None,
            toast: None,
            now: 0,
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
        self.layout();
        self.invalidate();
    }

    pub fn set_remote(&mut self, remote: bool) {
        if self.remote != remote {
            self.remote = remote;
            self.invalidate();
        }
    }

    /// New machine list from the library. Everything is repainted only when
    /// something visible changed.
    pub fn set_vms(&mut self, vms: Vec<VmView>) {
        if self.selected.as_ref().is_some_and(|s| !vms.iter().any(|v| &v.id == s)) {
            self.selected = None;
        }
        if self.selected.is_none() {
            self.selected = vms.first().map(|v| v.id.clone());
        }
        self.vms = vms;
        self.layout();
        self.invalidate();
    }

    /// A new guest screen for one machine: damages only that card and the
    /// detail preview, never the whole window.
    pub fn set_thumb(&mut self, id: &str, thumb: Thumb) {
        let Some(i) = self.vms.iter().position(|v| v.id == id) else { return };
        self.vms[i].thumb = Some(thumb);
        let rects: Vec<Rect> = self
            .hits
            .iter()
            .filter(|(_, h)| *h == Hit::Card(i))
            .map(|(r, _)| *r)
            .chain(self.selected_index().filter(|s| *s == i).map(|_| self.preview_rect()))
            .collect();
        self.damage.extend(rects);
    }

    pub fn set_toast(&mut self, t: Option<String>) {
        if self.toast != t {
            self.toast = t;
            let r = self.toast_rect();
            self.damage.push(r);
        }
    }

    fn visible(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        (0..self.vms.len())
            .filter(|&i| {
                q.is_empty()
                    || self.vms[i].name.to_lowercase().contains(&q)
                    || self.vms[i].os_label.to_lowercase().contains(&q)
            })
            .collect()
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected.as_ref().and_then(|s| self.vms.iter().position(|v| &v.id == s))
    }

    // ---------- layout ----------

    fn bar_h(&self) -> i32 {
        self.p(60.0)
    }
    /// The three caption buttons (Windows 11 chrome only).
    fn caption_rect(&self) -> Rect {
        if self.chrome != Chrome::Windows11 {
            return Rect::default();
        }
        let w = 3 * self.p(46.0);
        Rect::new(self.w - w, 0, w, self.bar_h() - 1)
    }

    /// Left inset for the macOS traffic lights.
    fn mac_inset(&self) -> i32 {
        if self.chrome == Chrome::MacInset { self.p(72.0) } else { 0 }
    }

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
        let s = self.search_rect();
        TitleGeometry {
            height: self.bar_h(),
            controls: vec![(s.x, s.right())],
            buttons_left: if c.is_empty() { i32::MAX } else { c.x },
            max_button: (c.x + bw, c.x + 2 * bw),
        }
    }

    /// The frame reported a change of the maximise button or window state.
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

    fn pane_w(&self) -> i32 {
        if self.selected_index().is_some() && self.w >= self.p(1100.0) { self.p(384.0) } else { 0 }
    }
    fn search_rect(&self) -> Rect {
        let w = self.p(560.0).min(self.w - self.p(520.0)).max(self.p(240.0));
        let h = self.p(38.0);
        Rect::new((self.w - w) / 2, (self.bar_h() - h) / 2, w, h)
    }
    fn new_rect(&self) -> Rect {
        let right = self.w - self.pane_w() - self.p(28.0);
        let w = self.p(148.0);
        Rect::new(right - w, self.bar_h() + self.p(22.0), w, self.p(36.0))
    }
    fn preview_rect(&self) -> Rect {
        let pw = self.pane_w();
        let x = self.w - pw + self.p(24.0);
        let w = pw - self.p(48.0);
        Rect::new(x, self.bar_h() + self.p(24.0), w, w * 9 / 16)
    }
    fn toast_rect(&self) -> Rect {
        let w = self.p(560.0).min(self.w - self.p(40.0));
        Rect::new((self.w - self.pane_w() - w) / 2, self.h - self.p(64.0), w, self.p(42.0))
    }
    fn grid(&self) -> (i32, i32, i32, i32, i32) {
        let x0 = self.p(28.0);
        let avail = self.w - self.pane_w() - 2 * x0;
        let gap = self.p(18.0);
        let min_w = self.p(296.0);
        let cols = ((avail + gap) / (min_w + gap)).max(1);
        let cw = (avail - gap * (cols - 1)) / cols;
        let thumb_h = (cw - self.p(16.0)) * 9 / 16;
        let ch = thumb_h + self.p(84.0);
        (x0, cols, cw, ch, gap)
    }

    fn layout(&mut self) {
        let mut hits = vec![(self.search_rect(), Hit::Search)];
        let vis = self.visible();
        if self.vms.is_empty() {
            for (i, r) in self.tile_rects().into_iter().enumerate() {
                hits.push((r, Hit::Tile(i)));
            }
        } else {
            hits.push((self.new_rect(), Hit::New));
            let (x0, cols, cw, ch, gap) = self.grid();
            let y0 = self.bar_h() + self.p(80.0);
            for (n, &i) in vis.iter().enumerate() {
                let (col, row) = (n as i32 % cols, n as i32 / cols);
                let r = Rect::new(x0 + col * (cw + gap), y0 + row * (ch + gap), cw, ch);
                hits.push((self.card_button(r), Hit::CardPrimary(i)));
                hits.push((r, Hit::Card(i)));
            }
            if self.pane_w() > 0 {
                let (a, b) = self.pane_buttons();
                hits.push((a, Hit::Primary));
                hits.push((b, Hit::Secondary));
            }
        }
        self.hits = hits;
    }

    fn thumb_rect(&self, card: Rect) -> Rect {
        let i = self.p(8.0);
        let w = card.w - 2 * i;
        Rect::new(card.x + i, card.y + i, w, w * 9 / 16)
    }
    fn card_button(&self, card: Rect) -> Rect {
        let t = self.thumb_rect(card);
        let d = self.p(52.0);
        Rect::new(t.x + (t.w - d) / 2, t.y + (t.h - d) / 2, d, d)
    }
    fn pane_buttons(&self) -> (Rect, Rect) {
        let pv = self.preview_rect();
        let y = pv.bottom() + self.p(96.0);
        let h = self.p(38.0);
        let gap = self.p(10.0);
        let w = (pv.w - gap) / 2;
        (Rect::new(pv.x, y, w, h), Rect::new(pv.x + w + gap, y, pv.w - w - gap, h))
    }
    fn tile_rects(&self) -> Vec<Rect> {
        let (tw, th, gap) = (self.p(196.0), self.p(150.0), self.p(16.0));
        let n = TILES.len() as i32 + 1;
        let cols = ((self.w - self.p(80.0) + gap) / (tw + gap)).clamp(1, n);
        let rows = (n + cols - 1) / cols;
        let total_w = cols * tw + (cols - 1) * gap;
        let x0 = (self.w - total_w) / 2;
        let y0 = (self.h / 2 - self.p(40.0)).max(self.bar_h() + self.p(200.0)) - (rows - 1) * (th + gap) / 2;
        (0..n).map(|i| Rect::new(x0 + (i % cols) * (tw + gap), y0 + (i / cols) * (th + gap), tw, th)).collect()
    }

    // ---------- input ----------

    fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    fn rect_of(&self, h: Hit) -> Option<Rect> {
        let card = match h {
            Hit::CardPrimary(i) => Some(Hit::Card(i)),
            _ => None,
        };
        self.hits.iter().find(|(_, x)| *x == card.unwrap_or(h)).map(|(r, _)| *r)
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
            Some(Hit::New | Hit::Card(_) | Hit::CardPrimary(_) | Hit::Primary | Hit::Secondary | Hit::Tile(_))
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
            return;
        }
        let was_focus = self.search_focus;
        self.search_focus = h == Some(Hit::Search);
        if was_focus != self.search_focus {
            self.damage.push(self.search_rect());
        }
        match h {
            Some(Hit::New) => self.actions.push(Action::NewFromIso { preset: None }),
            Some(Hit::Tile(i)) => {
                self.actions.push(Action::NewFromIso { preset: TILES.get(i).map(|t| t.0) });
            }
            Some(Hit::Card(i)) => self.select(i),
            Some(Hit::CardPrimary(i)) => {
                self.select(i);
                self.primary(i);
            }
            Some(Hit::Primary) => {
                if let Some(i) = self.selected_index() {
                    self.primary(i)
                }
            }
            Some(Hit::Secondary) => {
                if let Some(i) = self.selected_index() {
                    self.secondary(i)
                }
            }
            _ => {}
        }
    }

    fn select(&mut self, i: usize) {
        let id = self.vms[i].id.clone();
        if self.selected.as_deref() != Some(&id) {
            self.selected = Some(id);
            self.layout();
            self.invalidate();
        }
    }

    fn primary(&mut self, i: usize) {
        let id = self.vms[i].id.clone();
        match self.vms[i].state {
            RunState::Off | RunState::Failed(_) => self.actions.push(Action::Start { id, window: true }),
            RunState::Running { .. } => self.actions.push(Action::Stop { id, force: false }),
            RunState::Starting | RunState::Stopping => {}
        }
    }

    fn secondary(&mut self, i: usize) {
        let id = self.vms[i].id.clone();
        match self.vms[i].state {
            RunState::Off | RunState::Failed(_) => self.actions.push(Action::Start { id, window: false }),
            RunState::Running { .. } | RunState::Stopping => self.actions.push(Action::Stop { id, force: true }),
            RunState::Starting => {}
        }
    }

    /// Typed text goes to the search field (it filters as you type).
    pub fn text(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        if s.is_empty() {
            return;
        }
        self.search_focus = true;
        self.query.push_str(&s);
        self.after_query();
    }

    pub fn key_backspace(&mut self) {
        if self.query.pop().is_some() {
            self.after_query();
        }
    }

    pub fn key_escape(&mut self) {
        if !self.query.is_empty() || self.search_focus {
            self.query.clear();
            self.search_focus = false;
            self.after_query();
        }
    }

    pub fn key_focus_search(&mut self) {
        self.search_focus = true;
        self.damage.push(self.search_rect());
    }

    pub fn key_move(&mut self, d: i32) {
        let vis = self.visible();
        if vis.is_empty() {
            return;
        }
        let cur = self.selected_index().and_then(|s| vis.iter().position(|&v| v == s));
        let (_, cols, ..) = self.grid();
        let step = if d.abs() == 2 { cols * d.signum() } else { d };
        let n = match cur {
            Some(c) => (c as i32 + step).clamp(0, vis.len() as i32 - 1) as usize,
            None => 0,
        };
        self.select(vis[n]);
    }

    pub fn key_enter(&mut self) {
        if let Some(i) = self.selected_index() {
            self.primary(i);
        }
    }

    fn after_query(&mut self) {
        let vis = self.visible();
        if let Some(&first) = vis.first()
            && !self.selected_index().is_some_and(|s| vis.contains(&s))
        {
            self.selected = Some(self.vms[first].id.clone());
        }
        self.layout();
        self.invalidate();
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
        self.paint_bar(cv);
        if self.vms.is_empty() {
            self.paint_empty(cv);
        } else {
            self.paint_library(cv);
            if self.pane_w() > 0 {
                self.paint_pane(cv);
            }
        }
        if let Some(t) = self.toast.clone() {
            let r = self.toast_rect();
            cv.fill_round(r, self.fs(10.0), 0x2A1E22);
            cv.fill(Rect::new(r.x + self.p(14.0), r.y + r.h / 2 - self.p(4.0), self.p(8.0), self.p(8.0)), th::FAILED);
            let (f, s) = (self.f.regular, self.fs(13.0));
            self.te.draw_in(
                cv,
                f,
                s,
                Rect::new(r.x + self.p(22.0), r.y, r.w - self.p(30.0), r.h),
                self.fs(8.0),
                &t,
                th::TEXT,
            );
        }
    }

    fn paint_bar(&mut self, cv: &mut Canvas) {
        let bar = Rect::new(0, 0, self.w, self.bar_h());
        cv.fill(bar, th::BAR);
        cv.fill(Rect::new(0, bar.bottom() - 1, self.w, 1), th::BORDER);
        // Mark: a rounded square holding a ring with a tail (the Q).
        let m = self.p(26.0);
        let mr = Rect::new(self.mac_inset() + self.p(22.0), (bar.h - m) / 2, m, m);
        cv.fill_round(mr, self.fs(7.0), th::ACCENT);
        let (cx, cy) = (mr.x as f32 + m as f32 / 2.0, mr.y as f32 + m as f32 / 2.0);
        cv.ring(cx, cy, self.fs(6.6), self.fs(2.6), th::ACCENT_TEXT);
        cv.stroke(
            &[(cx + self.fs(3.0), cy + self.fs(3.0)), (cx + self.fs(7.2), cy + self.fs(7.2))],
            self.fs(2.6),
            th::ACCENT_TEXT,
        );
        let (semi, reg) = (self.f.semi, self.f.regular);
        let base = (bar.h as f32 / 2.0 + self.te.ascent(semi, self.fs(17.0)) * 0.36).round();
        self.te.draw(
            cv,
            semi,
            self.fs(17.0),
            mr.right() as f32 + self.fs(11.0),
            base,
            "Qorral",
            th::TEXT,
            200.0 * self.scale,
        );

        // Search: filters machines now; finds OS installs once the catalog lands.
        let s = self.search_rect();
        let focus = self.search_focus;
        let hover = self.hover == Some(Hit::Search);
        let border = if focus {
            th::ACCENT
        } else if hover {
            th::BORDER_STRONG
        } else {
            th::BORDER
        };
        cv.round_border(s, self.fs(10.0), border, th::FIELD);
        let (ix, iy) = (s.x as f32 + self.fs(20.0), s.y as f32 + s.h as f32 / 2.0 - self.fs(1.0));
        cv.ring(ix, iy, self.fs(6.0), self.fs(1.8), th::MUTED);
        cv.stroke(
            &[(ix + self.fs(4.4), iy + self.fs(4.4)), (ix + self.fs(8.2), iy + self.fs(8.2))],
            self.fs(1.9),
            th::MUTED,
        );
        let text_r = Rect::new(s.x + self.p(34.0), s.y, s.w - self.p(110.0), s.h);
        if self.query.is_empty() {
            let hint = if self.vms.is_empty() { "Search…" } else { "Search machines" };
            self.te.draw_in(cv, reg, self.fs(13.5), text_r, self.fs(4.0), hint, th::FAINT);
        } else {
            let q = self.query.clone();
            let end = self.te.draw_in(cv, reg, self.fs(13.5), text_r, self.fs(4.0), &q, th::TEXT);
            if focus {
                cv.fill(
                    Rect::new(end.round() as i32 + 1, s.y + self.p(10.0), self.p(1.5).max(1), s.h - self.p(20.0)),
                    th::ACCENT,
                );
            }
        }
        if focus && self.query.is_empty() {
            cv.fill(
                Rect::new(text_r.x + self.p(4.0), s.y + self.p(10.0), self.p(1.5).max(1), s.h - self.p(20.0)),
                th::ACCENT,
            );
        }
        // Keycap hint.
        let kc = Rect::new(s.right() - self.p(62.0), s.y + self.p(8.0), self.p(50.0), s.h - self.p(16.0));
        cv.round_border(kc, self.fs(5.0), th::BORDER_STRONG, th::FIELD);
        let kw = self.te.measure(reg, self.fs(11.0), "Ctrl K");
        let kb = (kc.y as f32 + kc.h as f32 / 2.0 + self.te.ascent(reg, self.fs(11.0)) * 0.36).round();
        self.te.draw(cv, reg, self.fs(11.0), kc.x as f32 + (kc.w as f32 - kw) / 2.0, kb, "Ctrl K", th::MUTED, kw + 2.0);

        // Host status, right-aligned: the accelerator and the QEMU build.
        let right = if self.caption_rect().is_empty() { self.w } else { self.caption_rect().x };
        let mut x = right as f32 - self.fs(18.0);
        let (ok, label) = if self.host.qemu_found {
            (true, format!("QEMU {}", self.host.qemu_version.clone().unwrap_or_default()))
        } else {
            (false, "QEMU not found".to_string())
        };
        let s13 = self.fs(12.5);
        let base = (bar.h as f32 / 2.0 + self.te.ascent(reg, s13) * 0.36).round();
        for (text, dot) in
            [(label, if ok { th::RUNNING } else { th::FAILED }), (self.host.accel.to_uppercase(), th::RUNNING)]
        {
            let w = self.te.measure(reg, s13, &text);
            x -= w;
            self.te.draw(cv, reg, s13, x, base, &text, th::MUTED, w + 2.0);
            x -= self.fs(10.0);
            cv.fill_circle(x, bar.h as f32 / 2.0, self.fs(3.2), dot);
            x -= self.fs(22.0);
        }
        self.paint_caption(cv);
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
                _ => (th::BAR, idle),
            };
            cv.fill(b, fill);
            let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
            match i {
                0 => cv.fill(Rect::new(cx - s, cy, 2 * s, line), glyph),
                1 if self.maximized => {
                    let k = self.p(2.0);
                    // Back window: only its top and right edges show.
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

    fn paint_library(&mut self, cv: &mut Canvas) {
        let (reg, semi) = (self.f.regular, self.f.semi);
        let x0 = self.p(28.0) as f32;
        let base = (self.bar_h() + self.p(48.0)) as f32;
        let end = self.te.draw(cv, semi, self.fs(22.0), x0, base, "Machines", th::TEXT, 400.0 * self.scale);
        let running = self.vms.iter().filter(|v| matches!(v.state, RunState::Running { .. })).count();
        let failed = self.vms.iter().filter(|v| matches!(v.state, RunState::Failed(_))).count();
        let mut x = end + self.fs(16.0);
        let mut chips: Vec<(String, u32)> = Vec::new();
        if running > 0 {
            chips.push((format!("{running} running"), th::RUNNING));
        }
        if failed > 0 {
            chips.push((format!("{failed} failed"), th::FAILED));
        }
        chips.push((format!("{} total", self.vms.len()), th::OFF));
        for (t, c) in chips {
            x = self.chip(cv, x, base, &t, c) + self.fs(8.0);
        }
        // New machine.
        let nr = self.new_rect();
        let hover = self.hover == Some(Hit::New);
        cv.fill_round(nr, self.fs(9.0), if hover { mix(th::ACCENT, 0xFFFFFF, 30) } else { th::ACCENT });
        let (px, py) = (nr.x as f32 + self.fs(20.0), nr.y as f32 + nr.h as f32 / 2.0);
        let a = self.fs(5.5);
        cv.stroke(&[(px - a, py), (px + a, py)], self.fs(2.0), th::ACCENT_TEXT);
        cv.stroke(&[(px, py - a), (px, py + a)], self.fs(2.0), th::ACCENT_TEXT);
        let tb = (py + self.te.ascent(semi, self.fs(13.5)) * 0.36).round();
        self.te.draw(cv, semi, self.fs(13.5), px + self.fs(14.0), tb, "New machine", th::ACCENT_TEXT, nr.w as f32);

        let vis = self.visible();
        if vis.is_empty() {
            let msg = format!("No machine matches “{}”", self.query.trim());
            self.te.draw(cv, reg, self.fs(14.0), x0, base + self.fs(64.0), &msg, th::MUTED, 900.0 * self.scale);
            return;
        }
        let cards: Vec<(Rect, usize)> =
            self.hits.iter().filter_map(|(r, h)| if let Hit::Card(i) = h { Some((*r, *i)) } else { None }).collect();
        let clip = cv.clip();
        for (r, i) in cards {
            if !r.intersect(clip).is_empty() {
                self.paint_card(cv, r, i);
            }
        }
    }

    fn chip(&mut self, cv: &mut Canvas, x: f32, base: f32, text: &str, dot: u32) -> f32 {
        let reg = self.f.regular;
        let s = self.fs(12.0);
        let w = self.te.measure(reg, s, text);
        let h = self.p(24.0);
        let r = Rect::new(x as i32, (base - self.fs(16.5)) as i32, (w + self.fs(28.0)) as i32, h);
        cv.fill_round(r, h as f32 / 2.0, th::CARD);
        cv.fill_circle(r.x as f32 + self.fs(11.0), r.y as f32 + h as f32 / 2.0, self.fs(3.2), dot);
        let b = (r.y as f32 + h as f32 / 2.0 + self.te.ascent(reg, s) * 0.36).round();
        self.te.draw(cv, reg, s, r.x as f32 + self.fs(20.0), b, text, th::MUTED, w + 2.0);
        r.right() as f32
    }

    fn state_style(&self, s: &RunState) -> (String, u32) {
        match s {
            RunState::Off => ("Off".into(), th::OFF),
            RunState::Starting => ("Starting".into(), th::BUSY),
            RunState::Stopping => ("Shutting down".into(), th::BUSY),
            RunState::Failed(_) => ("Failed".into(), th::FAILED),
            RunState::Running { since } => {
                (format!("Running · {}", uptime(self.now.saturating_sub(*since))), th::RUNNING)
            }
        }
    }

    fn specs(v: &VmView) -> String {
        let mem = if v.memory_mib.is_multiple_of(1024) {
            format!("{} GB", v.memory_mib / 1024)
        } else {
            format!("{} MB", v.memory_mib)
        };
        format!("{} · {} vCPU · {mem} · {}", v.os_label, v.cpus, v.gpu.label())
    }

    fn paint_card(&mut self, cv: &mut Canvas, r: Rect, i: usize) {
        let (reg, semi) = (self.f.regular, self.f.semi);
        let hover = matches!(self.hover, Some(Hit::Card(j) | Hit::CardPrimary(j)) if j == i);
        let selected = self.selected_index() == Some(i);
        let v = self.vms[i].clone();
        let bg = if hover { th::CARD_HOVER } else { th::CARD };
        let border = if selected {
            th::ACCENT
        } else if hover {
            th::BORDER_STRONG
        } else {
            th::BORDER
        };
        cv.round_border(r, self.fs(14.0), border, bg);
        if selected {
            cv.round_border(r.inset(1, 1), self.fs(13.0), th::ACCENT, bg);
        }
        let t = self.thumb_rect(r);
        self.paint_screen(cv, t, &v, self.fs(9.0));
        if hover && !matches!(v.state, RunState::Starting | RunState::Stopping) {
            let b = self.card_button(r);
            let on_button = self.hover == Some(Hit::CardPrimary(i));
            if !self.remote {
                cv.fill_alpha(t, 0x000000, 70);
            }
            let fill = if on_button { th::ACCENT } else { 0xF2F4F8 };
            let icon = if on_button { th::ACCENT_TEXT } else { 0x10131A };
            let (cx, cy, rad) = (b.x as f32 + b.w as f32 / 2.0, b.y as f32 + b.h as f32 / 2.0, b.w as f32 / 2.0);
            cv.fill_circle(cx, cy, rad, fill);
            if matches!(v.state, RunState::Running { .. }) {
                // Shut down: a power glyph.
                cv.ring(cx, cy + self.fs(1.0), self.fs(9.0), self.fs(2.4), icon);
                cv.fill(
                    Rect::new((cx - self.fs(3.5)) as i32, (cy - self.fs(13.0)) as i32, self.p(7.0), self.p(8.0)),
                    fill,
                );
                cv.stroke(&[(cx, cy - self.fs(11.0)), (cx, cy - self.fs(2.0))], self.fs(2.4), icon);
            } else {
                let k = self.fs(10.0);
                cv.fill_triangle((cx - k * 0.7, cy - k), (cx - k * 0.7, cy + k), (cx + k, cy), icon);
            }
            let label = if matches!(v.state, RunState::Running { .. }) { "Shut down" } else { "Start" };
            let lw = self.te.measure(semi, self.fs(12.5), label);
            let lb = b.bottom() as f32 + self.fs(22.0);
            if !self.remote {
                let pill = Rect::new(
                    (cx - lw / 2.0 - self.fs(10.0)) as i32,
                    (lb - self.fs(16.0)) as i32,
                    (lw + self.fs(20.0)) as i32,
                    self.p(23.0),
                );
                cv.fill_round(pill, self.fs(11.5), 0x0B0D11);
            }
            self.te.draw(cv, semi, self.fs(12.5), cx - lw / 2.0, lb, label, 0xF2F4F8, lw + 2.0);
        }
        // Name and state.
        let nb = t.bottom() as f32 + self.fs(30.0);
        let (label, color) = self.state_style(&v.state);
        let pw = self.te.measure(semi, self.fs(12.0), &label) + self.fs(28.0);
        let pill = Rect::new(
            (r.right() as f32 - self.fs(14.0) - pw) as i32,
            (nb - self.fs(16.5)) as i32,
            pw as i32,
            self.p(23.0),
        );
        cv.fill_round(pill, pill.h as f32 / 2.0, mix(th::CARD, color, 34));
        cv.fill_circle(pill.x as f32 + self.fs(11.0), pill.y as f32 + pill.h as f32 / 2.0, self.fs(3.3), color);
        self.te.draw(
            cv,
            semi,
            self.fs(12.0),
            pill.x as f32 + self.fs(19.0),
            nb - self.fs(1.0),
            &label,
            mix(color, 0xFFFFFF, 90),
            pw,
        );
        let name_w = pill.x as f32 - r.x as f32 - self.fs(30.0);
        self.te.draw(cv, semi, self.fs(15.5), r.x as f32 + self.fs(14.0), nb, &v.name, th::TEXT, name_w);
        let (line, col) = match &v.state {
            RunState::Failed(msg) => (msg.clone(), mix(th::FAILED, th::TEXT, 60)),
            _ => (Self::specs(&v), th::MUTED),
        };
        self.te.draw(
            cv,
            reg,
            self.fs(12.5),
            r.x as f32 + self.fs(14.0),
            nb + self.fs(24.0),
            &line,
            col,
            r.w as f32 - self.fs(28.0),
        );
    }

    /// The guest screen: live, the last one seen (dimmed when off), or a
    /// poster for machines that have never shown one.
    fn paint_screen(&mut self, cv: &mut Canvas, t: Rect, v: &VmView, radius: f32) {
        let semi = self.f.semi;
        match &v.thumb {
            Some(img) if img.w > 0 && img.h > 0 => {
                cv.fill_round(t, radius, th::SCREEN);
                // Fit inside, keep the aspect ratio.
                let (sw, sh) = (t.w as f32 / img.w as f32, t.h as f32 / img.h as f32);
                let k = sw.min(sh);
                let (dw, dh) = ((img.w as f32 * k) as i32, (img.h as f32 * k) as i32);
                let d = Rect::new(t.x + (t.w - dw) / 2, t.y + (t.h - dh) / 2, dw, dh);
                let old = cv.set_clip(cv.clip().intersect(t.inset(1, 1)));
                cv.draw_rgba_scaled(d, img.w, img.h, &img.rgba);
                let off = !matches!(v.state, RunState::Running { .. });
                if off && !self.remote {
                    cv.fill_alpha(d, th::BG, 150);
                }
                cv.set_clip(old);
                if off {
                    let s = self.fs(11.5);
                    let label = "Last screen";
                    let w = self.te.measure(self.f.semi, s, label);
                    let tag =
                        Rect::new(t.x + self.p(10.0), t.y + self.p(10.0), (w + self.fs(18.0)) as i32, self.p(22.0));
                    cv.fill_round(tag, self.fs(6.0), 0x0B0D11);
                    let b = (tag.y as f32 + tag.h as f32 / 2.0 + self.te.ascent(self.f.semi, s) * 0.36).round();
                    self.te.draw(cv, self.f.semi, s, tag.x as f32 + self.fs(9.0), b, label, th::MUTED, w + 2.0);
                }
            }
            _ => {
                let tint = th::os_tint(&v.os_label);
                cv.fill_round(t, radius, mix(th::SCREEN, tint, 26));
                let (cx, cy) = (t.x as f32 + t.w as f32 / 2.0, t.y as f32 + t.h as f32 / 2.0 - self.fs(8.0));
                let rad = (t.h as f32 * 0.2).min(self.fs(34.0));
                cv.fill_circle(cx, cy, rad, mix(th::SCREEN, tint, 150));
                let ini = initials(&v.os_label);
                let s = rad * 0.78;
                let w = self.te.measure(semi, s, &ini);
                let b = (cy + self.te.ascent(semi, s) * 0.36).round();
                self.te.draw(cv, semi, s, cx - w / 2.0, b, &ini, 0xFFFFFF, w + 2.0);
                let hint = match v.state {
                    RunState::Starting => "Starting…",
                    RunState::Running { .. } => "Waiting for the first frame",
                    RunState::Failed(_) => "Did not start",
                    _ => "Not started yet",
                };
                let hs = self.fs(12.0);
                let hw = self.te.measure(self.f.regular, hs, hint);
                self.te.draw(
                    cv,
                    self.f.regular,
                    hs,
                    cx - hw / 2.0,
                    cy + rad + self.fs(24.0),
                    hint,
                    mix(th::MUTED, tint, 60),
                    hw + 2.0,
                );
            }
        }
    }

    fn paint_pane(&mut self, cv: &mut Canvas) {
        let Some(i) = self.selected_index() else { return };
        let v = self.vms[i].clone();
        let (reg, semi, mono) = (self.f.regular, self.f.semi, self.f.mono);
        let pw = self.pane_w();
        let pane = Rect::new(self.w - pw, self.bar_h(), pw, self.h - self.bar_h());
        cv.fill(pane, th::PANEL);
        cv.fill(Rect::new(pane.x, pane.y, 1, pane.h), th::BORDER);
        let pv = self.preview_rect();
        self.paint_screen(cv, pv, &v, self.fs(10.0));
        let x = pv.x as f32;
        let mut y = pv.bottom() as f32 + self.fs(38.0);
        self.te.draw(cv, semi, self.fs(20.0), x, y, &v.name, th::TEXT, pv.w as f32);
        y += self.fs(26.0);
        let (label, color) = self.state_style(&v.state);
        cv.fill_circle(x + self.fs(4.0), y - self.fs(4.5), self.fs(3.6), color);
        self.te.draw(cv, reg, self.fs(13.0), x + self.fs(14.0), y, &label, th::MUTED, pv.w as f32);
        if let RunState::Failed(msg) = &v.state {
            y += self.fs(20.0);
            self.te.draw(cv, reg, self.fs(12.0), x, y, msg, th::FAILED, pv.w as f32);
        }
        // Actions.
        let (a, b) = self.pane_buttons();
        let running = matches!(v.state, RunState::Running { .. } | RunState::Stopping);
        let busy = matches!(v.state, RunState::Starting | RunState::Stopping);
        let (pl, sl) = if running { ("Shut down", "Force off") } else { ("Start", "Start in background") };
        self.button(cv, a, pl, true, self.hover == Some(Hit::Primary), busy);
        self.button(cv, b, sl, false, self.hover == Some(Hit::Secondary), v.state == RunState::Starting);

        // Hardware.
        y = a.bottom() as f32 + self.fs(40.0);
        self.te.draw(cv, semi, self.fs(12.0), x, y, "HARDWARE", th::FAINT, pv.w as f32);
        y += self.fs(10.0);
        let mem = if v.memory_mib.is_multiple_of(1024) {
            format!("{} GB", v.memory_mib / 1024)
        } else {
            format!("{} MB", v.memory_mib)
        };
        let rows: Vec<(&str, String)> = vec![
            ("System", v.os_label.clone()),
            ("Processors", format!("{} virtual CPUs", v.cpus)),
            ("Memory", mem),
            ("Disk", format!("{} GB · {}", v.disk_gib, if v.nvme { "NVMe" } else { "virtio" })),
            ("Graphics", v.gpu.label().to_string()),
            ("Firmware", if v.uefi { "UEFI".into() } else { "BIOS".into() }),
            (
                "Network",
                if v.forwards == 0 {
                    "NAT".into()
                } else {
                    format!("NAT · {} port forward{}", v.forwards, if v.forwards == 1 { "" } else { "s" })
                },
            ),
        ];
        for (k, val) in rows {
            y += self.fs(28.0);
            cv.fill(Rect::new(pv.x, (y - self.fs(19.0)) as i32, pv.w, 1), th::BORDER);
            self.te.draw(cv, reg, self.fs(13.0), x, y, k, th::MUTED, self.fs(110.0));
            self.te.draw(cv, reg, self.fs(13.0), x + self.fs(110.0), y, &val, th::TEXT, pv.w as f32 - self.fs(110.0));
        }
        // Command line (power users): the exact QEMU invocation, wrapped.
        y += self.fs(40.0);
        self.te.draw(cv, semi, self.fs(12.0), x, y, "QEMU COMMAND", th::FAINT, pv.w as f32);
        let hint = format!("qorral info {}", v.id);
        let hw = self.te.measure(mono, self.fs(11.5), &hint);
        self.te.draw(cv, mono, self.fs(11.5), pv.right() as f32 - hw, y, &hint, th::ACCENT, hw + 2.0);
        y += self.fs(10.0);
        let max_h = (pane.bottom() as f32 - y - self.fs(24.0)).max(0.0);
        let s = self.fs(11.5);
        let cw = self.te.measure(mono, s, "m").max(1.0);
        let per_line = ((pv.w as f32 - self.fs(24.0)) / cw).floor().max(10.0) as usize;
        let chars: Vec<char> = v.command.chars().collect();
        let need = chars.len().div_ceil(per_line) as f32 * self.fs(17.0) + self.fs(22.0);
        let box_h = need.min(max_h);
        if box_h > self.fs(40.0) {
            let bx = Rect::new(pv.x, y as i32, pv.w, box_h as i32);
            cv.round_border(bx, self.fs(8.0), th::BORDER, th::BG);
            let lines = ((box_h - self.fs(8.0)) / self.fs(17.0)) as usize + 1;
            let mut ly = y + self.fs(22.0);
            for (n, chunk) in chars.chunks(per_line).take(lines.saturating_sub(1)).enumerate() {
                let line: String = chunk.iter().collect();
                let col = if n == 0 { th::TEXT } else { th::MUTED };
                self.te.draw(cv, mono, s, x + self.fs(12.0), ly, &line, col, bx.w as f32);
                ly += self.fs(17.0);
            }
        }
    }

    fn button(&mut self, cv: &mut Canvas, r: Rect, label: &str, primary: bool, hover: bool, busy: bool) {
        let semi = self.f.semi;
        let (bg, fg) = match (primary, busy) {
            (_, true) => (th::CARD, th::FAINT),
            (true, _) => (if hover { mix(th::ACCENT, 0xFFFFFF, 30) } else { th::ACCENT }, th::ACCENT_TEXT),
            (false, _) => (if hover { th::CARD_HOVER } else { th::CARD }, th::TEXT),
        };
        if primary || busy {
            cv.fill_round(r, self.fs(9.0), bg);
        } else {
            cv.round_border(r, self.fs(9.0), if hover { th::BORDER_STRONG } else { th::BORDER }, bg);
        }
        let s = self.fs(13.0);
        let w = self.te.measure(semi, s, label);
        let b = (r.y as f32 + r.h as f32 / 2.0 + self.te.ascent(semi, s) * 0.36).round();
        self.te.draw(cv, semi, s, r.x as f32 + (r.w as f32 - w) / 2.0, b, label, fg, r.w as f32);
    }

    fn paint_empty(&mut self, cv: &mut Canvas) {
        let (reg, semi) = (self.f.regular, self.f.semi);
        let tiles = self.tile_rects();
        let Some(first) = tiles.first().copied() else { return };
        let cx = self.w as f32 / 2.0;
        let title = "Create your first machine";
        let tw = self.te.measure(semi, self.fs(28.0), title);
        let ty = first.y as f32 - self.fs(78.0);
        self.te.draw(cv, semi, self.fs(28.0), cx - tw / 2.0, ty, title, th::TEXT, tw + 2.0);
        let sub = "Pick a system and point Qorral at its install image. It sets up the rest.";
        let sw = self.te.measure(reg, self.fs(14.5), sub);
        self.te.draw(cv, reg, self.fs(14.5), cx - sw / 2.0, ty + self.fs(32.0), sub, th::MUTED, sw + 2.0);
        for (i, r) in tiles.into_iter().enumerate() {
            let hover = self.hover == Some(Hit::Tile(i));
            if let Some((_, name, line)) = TILES.get(i) {
                let tint = th::os_tint(name);
                cv.round_border(
                    r,
                    self.fs(14.0),
                    if hover { th::BORDER_STRONG } else { th::BORDER },
                    if hover { th::CARD_HOVER } else { th::CARD },
                );
                let (ccx, ccy) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + self.fs(52.0));
                cv.fill_circle(ccx, ccy, self.fs(24.0), mix(th::CARD, tint, if hover { 210 } else { 170 }));
                let ini = initials(name);
                let iw = self.te.measure(semi, self.fs(17.0), &ini);
                let ib = (ccy + self.te.ascent(semi, self.fs(17.0)) * 0.36).round();
                self.te.draw(cv, semi, self.fs(17.0), ccx - iw / 2.0, ib, &ini, 0xFFFFFF, iw + 2.0);
                let nw = self.te.measure(semi, self.fs(14.5), name);
                self.te.draw(
                    cv,
                    semi,
                    self.fs(14.5),
                    ccx - nw / 2.0,
                    r.y as f32 + self.fs(108.0),
                    name,
                    th::TEXT,
                    nw + 2.0,
                );
                let lw = self.te.measure(reg, self.fs(12.0), line);
                self.te.draw(
                    cv,
                    reg,
                    self.fs(12.0),
                    ccx - lw / 2.0,
                    r.y as f32 + self.fs(128.0),
                    line,
                    th::MUTED,
                    lw + 2.0,
                );
            } else {
                // "Any image": a dashed outline in solid steps (no alpha, RDP-friendly).
                cv.fill_round(r, self.fs(14.0), if hover { th::CARD } else { th::BG });
                dashed_round(cv, r, if hover { th::ACCENT } else { th::BORDER_STRONG }, self.scale);
                let (ccx, ccy) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + self.fs(52.0));
                let a = self.fs(9.0);
                let col = if hover { th::ACCENT } else { th::MUTED };
                cv.stroke(&[(ccx - a, ccy), (ccx + a, ccy)], self.fs(2.2), col);
                cv.stroke(&[(ccx, ccy - a), (ccx, ccy + a)], self.fs(2.2), col);
                let n = "Any other system";
                let nw = self.te.measure(semi, self.fs(14.5), n);
                self.te.draw(
                    cv,
                    semi,
                    self.fs(14.5),
                    ccx - nw / 2.0,
                    r.y as f32 + self.fs(108.0),
                    n,
                    th::TEXT,
                    nw + 2.0,
                );
                let l = "Choose an ISO file";
                let lw = self.te.measure(reg, self.fs(12.0), l);
                self.te.draw(
                    cv,
                    reg,
                    self.fs(12.0),
                    ccx - lw / 2.0,
                    r.y as f32 + self.fs(128.0),
                    l,
                    th::MUTED,
                    lw + 2.0,
                );
            }
        }
        let foot = "Or from a terminal:  qorral create my-vm --iso path/to/image.iso";
        let fw = self.te.measure(self.f.mono, self.fs(12.0), foot);
        let last = self.tile_rects().last().copied().unwrap_or(first);
        self.te.draw(
            cv,
            self.f.mono,
            self.fs(12.0),
            cx - fw / 2.0,
            last.bottom() as f32 + self.fs(48.0),
            foot,
            th::FAINT,
            fw + 2.0,
        );
    }
}

fn dashed_round(cv: &mut Canvas, r: Rect, color: u32, scale: f32) {
    let (dash, gap) = ((6.0 * scale) as i32, (5.0 * scale) as i32);
    let inset = (10.0 * scale) as i32;
    let mut x = r.x + inset;
    while x < r.right() - inset {
        let w = dash.min(r.right() - inset - x);
        cv.fill(Rect::new(x, r.y, w, 1), color);
        cv.fill(Rect::new(x, r.bottom() - 1, w, 1), color);
        x += dash + gap;
    }
    let mut y = r.y + inset;
    while y < r.bottom() - inset {
        let h = dash.min(r.bottom() - inset - y);
        cv.fill(Rect::new(r.x, y, 1, h), color);
        cv.fill(Rect::new(r.right() - 1, y, 1, h), color);
        y += dash + gap;
    }
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
    fn initials_and_uptime_read_well() {
        assert_eq!(initials("Windows 11"), "W11");
        assert_eq!(initials("Arch Linux"), "AL");
        assert_eq!(initials("fedora"), "Fe");
        assert_eq!(uptime(30), "just now");
        assert_eq!(uptime(3 * 3600 + 120), "3 h 2 min");
    }
}
