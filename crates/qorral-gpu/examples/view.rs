//! Live zero-copy viewer (test): a window showing a running VM's display straight from the
//! guest's GPU memory, presented the moment each guest frame flips, with keyboard and mouse
//! forwarded. Closes after SECONDS and prints where the time went.
//!
//! `cargo run -p qorral-gpu --example view -- QMP_PORT QEMU_PID SECONDS [immediate|mailbox|fifo]`

#[cfg(windows)]
fn main() {
    use qorral_display::{Button, Caps, Display, Event};
    use qorral_gpu::{Compositor, GuestMemory, ImageParams, Layout, NativeWindow, PresentMode};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use winit::application::ApplicationHandler;
    use winit::dpi::PhysicalSize;
    use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::scancode::PhysicalKeyExtScancode;
    use winit::window::{Window, WindowId};

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: view QMP_PORT QEMU_PID SECONDS [immediate|mailbox|fifo]");
        std::process::exit(2);
    }
    let port: u16 = args[0].parse().expect("port");
    let pid: u32 = args[1].parse().expect("pid");
    let deadline = Instant::now() + Duration::from_secs(args[2].parse().expect("seconds"));
    let prefer = match args.get(3).map(String::as_str) {
        Some("mailbox") => PresentMode::Mailbox,
        Some("fifo") => PresentMode::Fifo,
        _ => PresentMode::Immediate,
    };

    /// Everything the listener thread (frames) and the window thread (resize, input) share.
    #[derive(Default)]
    struct State {
        comp: Option<Compositor>,
        window: Option<NativeWindow>,
        window_size: (u32, u32),
        resized: bool,
        guest: (u32, u32),
        last: u32,
        flip_to_presented_ms: Vec<f64>,
        gpu_ms: Vec<f64>,
        acquire_ms: Vec<f64>,
        imports: (u32, u32),
        errors: Vec<String>,
        mode: Option<PresentMode>,
    }

    /// The guest display fitted into the window, centred, aspect kept.
    fn fit(win: (u32, u32), guest: (u32, u32)) -> [f32; 4] {
        if guest.0 == 0 || guest.1 == 0 {
            return [0.0, 0.0, win.0 as f32, win.1 as f32];
        }
        let s = (win.0 as f32 / guest.0 as f32).min(win.1 as f32 / guest.1 as f32);
        let (w, h) = (guest.0 as f32 * s, guest.1 as f32 * s);
        [((win.0 as f32 - w) / 2.0).round(), ((win.1 as f32 - h) / 2.0).round(), w.round(), h.round()]
    }

    fn present_last(st: &mut State) {
        let (last, dst) = (st.last, fit(st.window_size, st.guest));
        if let Some(c) = st.comp.as_mut() {
            if st.resized {
                st.resized = false;
                let (w, h) = st.window_size;
                if let Err(e) = c.resize(w, h) {
                    st.errors.push(e.to_string());
                }
            }
            if last != 0
                && c.has(last)
                && let Err(e) = c.present(last, dst)
            {
                st.errors.push(e.to_string());
            }
        }
    }

    struct App {
        state: Arc<Mutex<State>>,
        window: Option<Window>,
        display: Option<Display>,
        port: u16,
        pid: u32,
        prefer: PresentMode,
        deadline: Instant,
        cursor: (f64, f64),
    }

    impl App {
        fn guest_point(&self) -> Option<(u32, u32)> {
            let st = self.state.lock().ok()?;
            let r = fit(st.window_size, st.guest);
            let (x, y) = ((self.cursor.0 as f32 - r[0]) / r[2], (self.cursor.1 as f32 - r[1]) / r[3]);
            if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) {
                return None;
            }
            Some(((x * st.guest.0 as f32) as u32, (y * st.guest.1 as f32) as u32))
        }
    }

    impl ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.window.is_some() {
                return;
            }
            let attrs = Window::default_attributes()
                .with_title(format!("Qorral zero-copy test (QEMU {})", self.pid))
                .with_inner_size(PhysicalSize::new(1280, 800));
            let window = event_loop.create_window(attrs).expect("window");
            let Ok(RawWindowHandle::Win32(h)) = window.window_handle().map(|h| h.as_raw()) else {
                panic!("not a Win32 window");
            };
            let native = NativeWindow { hwnd: h.hwnd.get(), hinstance: h.hinstance.map(|i| i.get()).unwrap_or(0) };
            {
                let mut st = self.state.lock().unwrap();
                st.window = Some(native);
                let s = window.inner_size();
                st.window_size = (s.width, s.height);
            }
            self.window = Some(window);

            // Attach the display: frames are presented on the listener thread as they arrive.
            let s = std::net::TcpStream::connect(("127.0.0.1", self.port)).expect("QMP connect");
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut line = String::new();
            let mut qmp = |cmd: &str, args: &[(&str, &str)]| -> Result<(), String> {
                let a: Vec<String> = args.iter().map(|(k, v)| format!("\"{k}\":\"{v}\"")).collect();
                writeln!(w, "{{\"execute\":\"{cmd}\",\"arguments\":{{{}}}}}", a.join(","))
                    .map_err(|e| e.to_string())?;
                loop {
                    line.clear();
                    r.read_line(&mut line).map_err(|e| e.to_string())?;
                    if line.contains("\"return\"") {
                        return Ok(());
                    }
                    if line.contains("\"error\"") {
                        return Err(line.trim().to_string());
                    }
                }
            };
            qmp("qmp_capabilities", &[]).expect("qmp_capabilities");
            let state = self.state.clone();
            let prefer = self.prefer;
            let display = Display::attach(self.pid, 0, Caps { vulkan: true, ..Caps::default() }, &mut qmp, move |e| {
                let t_recv = Instant::now();
                let mut guard = state.lock().unwrap();
                let st = &mut *guard;
                match e {
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
                        if st.comp.is_none()
                            && let Some(win) = st.window
                        {
                            let (w, h) = st.window_size;
                            match Compositor::for_window(&device_uuid, win, w, h, prefer) {
                                Ok(c) => {
                                    st.mode = c.present_mode();
                                    st.comp = Some(c);
                                }
                                Err(err) => st.errors.push(err.to_string()),
                            }
                        }
                        let image = image.map(|i| ImageParams {
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
                        });
                        let mem = GuestMemory {
                            handle: handle as isize,
                            allocation_size,
                            memory_type_index,
                            layout: Layout { offset, width, height, stride },
                            image,
                        };
                        if let Some(c) = st.comp.as_mut() {
                            match c.import(resource_id, mem) {
                                Ok(zero_copy) => {
                                    st.imports.0 += 1;
                                    st.imports.1 += u32::from(zero_copy);
                                }
                                Err(err) => st.errors.push(err.to_string()),
                            }
                        }
                        st.guest = (width, height);
                    }
                    Some(Event::FlipBlob { resource_id, .. }) if resource_id != 0 => {
                        st.last = resource_id;
                        let before = st.errors.len();
                        let timing = {
                            let dst = fit(st.window_size, st.guest);
                            if st.resized {
                                present_last(st);
                                None
                            } else if let Some(c) = st.comp.as_mut() {
                                c.present(resource_id, dst).map_err(|e| st.errors.push(e.to_string())).ok()
                            } else {
                                None
                            }
                        };
                        if let Some(t) = timing
                            && st.errors.len() == before
                        {
                            st.flip_to_presented_ms.push(t_recv.elapsed().as_secs_f64() * 1e3);
                            st.gpu_ms.push(t.gpu.as_secs_f64() * 1e3);
                            st.acquire_ms.push(t.acquire.as_secs_f64() * 1e3);
                        }
                    }
                    Some(Event::ForgetBlob { resource_id }) => {
                        if let Some(c) = st.comp.as_mut() {
                            c.forget(resource_id);
                        }
                    }
                    _ => {}
                }
            })
            .expect("attach");
            self.display = Some(display);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => event_loop.exit(),
                WindowEvent::Resized(s) => {
                    let mut st = self.state.lock().unwrap();
                    st.window_size = (s.width, s.height);
                    st.resized = true;
                    present_last(&mut st);
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if let (Some(d), Some(sc)) = (&self.display, event.physical_key.to_scancode()) {
                        // Windows scancodes: 0xE0xx for extended keys; QEMU's qnum sets 0x80.
                        let qnum = if sc & 0xff00 == 0xe000 { 0x80 | (sc & 0x7f) } else { sc & 0xff };
                        let _ = d.key(qnum, event.state == ElementState::Pressed);
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    self.cursor = (position.x, position.y);
                    if let (Some(d), Some((x, y))) = (&self.display, self.guest_point()) {
                        let _ = d.mouse_to(x, y);
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let b = match button {
                        MouseButton::Left => Button::Left,
                        MouseButton::Right => Button::Right,
                        MouseButton::Middle => Button::Middle,
                        MouseButton::Back => Button::Side,
                        MouseButton::Forward => Button::Extra,
                        MouseButton::Other(_) => return,
                    };
                    if let Some(d) = &self.display {
                        let _ = d.button(b, state == ElementState::Pressed);
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let up = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                        MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
                    };
                    if let Some(d) = &self.display {
                        let b = if up { Button::WheelUp } else { Button::WheelDown };
                        let _ = d.button(b, true);
                        let _ = d.button(b, false);
                    }
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if Instant::now() >= self.deadline {
                event_loop.exit();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
            }
        }
    }

    let state = Arc::new(Mutex::new(State::default()));
    let mut app =
        App { state: state.clone(), window: None, display: None, port, pid, prefer, deadline, cursor: (0.0, 0.0) };
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.run_app(&mut app).expect("run");
    // Detach first (stops frames), then drop the compositor before the window.
    drop(app.display.take());
    let mut st = state.lock().unwrap();
    let comp = st.comp.take();
    drop(comp);
    drop(app.window.take());

    println!("present mode: {:?}; guest buffers imported: {} ({} zero copy)", st.mode, st.imports.0, st.imports.1);
    for e in st.errors.iter().take(5) {
        println!("error: {e}");
    }
    let stats = |name: &str, v: &[f64]| {
        if v.is_empty() {
            return;
        }
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        println!(
            "{name}: {} frames, median {:.3} ms, p99 {:.3} ms, max {:.3} ms",
            s.len(),
            s[s.len() / 2],
            s[(s.len() * 99 / 100).min(s.len() - 1)],
            s[s.len() - 1]
        );
    };
    stats("flip received -> presented (GPU done)", &st.flip_to_presented_ms);
    stats("  acquire", &st.acquire_ms);
    stats("  record + GPU", &st.gpu_ms);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the view example is Windows-only for now");
}
